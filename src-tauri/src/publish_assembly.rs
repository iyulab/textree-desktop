//! What a publish sends, built from the last version added of each note — never from the files
//! as they are in the folder. Only chosen notes go out, with only the files they link to.

use crate::git_engine::{self, VaultRepo, NOTES_REF};
use crate::note_refs::{self, is_note_path, Target};
use crate::publish_scope;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assembly {
    /// Every file that goes out: (path relative to the notes folder, content).
    pub files: Vec<(String, Vec<u8>)>,
    /// Included notes going out.
    pub notes: Vec<String>,
    /// Files they link to that go out.
    pub assets: Vec<String>,
    /// Included notes with no version: left out.
    pub never_recorded: Vec<String>,
    /// Files linked to but not in the last versions: left out.
    pub missing: Vec<String>,
    /// Notes linked from published ones that do not go out.
    pub unpublished_links: Vec<String>,
    /// Linked files left out because their own nearest choice is "leave out".
    pub withheld: Vec<String>,
    /// Linked files left out because they are over [`MAX_LINKED_FILE_BYTES`].
    pub too_large: Vec<String>,
    pub nothing_chosen: bool,
}

/// The largest file a note brings along when a version of it is added, and that is published:
/// 50 MB, under what common site hosts accept for a single file. A note itself is never held to it.
pub const MAX_LINKED_FILE_BYTES: u64 = 50 * 1024 * 1024;

fn hidden(rel: &str) -> bool {
    rel.split('/').any(|s| s.starts_with('.'))
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Notes on disk under `root` (relative, `/`-separated) — only to name the chosen ones that have
/// no version yet.
fn notes_on_disk(root: &Path) -> Result<Vec<String>, String> {
    crate::publish::outgoing_notes(root).map_err(|e| e.to_string())
}

pub fn assemble(prepared: &VaultRepo, root: &Path) -> Result<Assembly, String> {
    assemble_capped(prepared, root, MAX_LINKED_FILE_BYTES)
}

/// [`assemble`], with linked files over `cap` bytes left out.
fn assemble_capped(prepared: &VaultRepo, root: &Path, cap: u64) -> Result<Assembly, String> {
    let repo = prepared.repo();
    let err = |e: git2::Error| e.message().to_string();
    let vault_prefix = git_engine::slashed(&prepared.path_in_repo(Path::new("")))
        .trim_end_matches('/')
        .to_string();
    let choices = publish_scope::for_vault(
        &publish_scope::read(repo, publish_scope::PUBLIC_REF).map_err(err)?,
        &vault_prefix,
    );
    let mut out = Assembly {
        nothing_chosen: choices.iter().all(|c| !c.include),
        ..Assembly::default()
    };
    if out.nothing_chosen {
        return Ok(out);
    }

    // Everything recorded inside this folder, relative to it.
    let in_vault = |in_repo: &str| -> Option<String> {
        if vault_prefix.is_empty() {
            Some(in_repo.to_string())
        } else {
            in_repo
                .strip_prefix(&format!("{vault_prefix}/"))
                .map(str::to_string)
        }
    };
    // Recorded, and still in the folder: a note or file the person deleted does not go out, even
    // though its last version stays in the history (that is what Deleted notes brings back).
    let on_disk = |rel: &str| {
        let mut path = root.to_path_buf();
        path.extend(rel.split('/'));
        path.is_file()
    };
    let recorded: BTreeSet<String> = git_engine::tip_paths(repo, NOTES_REF)
        .map_err(err)?
        .iter()
        .filter_map(|p| in_vault(p))
        .filter(|p| !hidden(p) && on_disk(p))
        .collect();
    let by_name: BTreeMap<String, Vec<String>> =
        recorded.iter().fold(BTreeMap::new(), |mut m, p| {
            m.entry(file_name(p).to_ascii_lowercase())
                .or_default()
                .push(p.clone());
            m
        });
    let read = |rel: &str| -> Result<Option<Vec<u8>>, String> {
        git_engine::content_at_tip(repo, NOTES_REF, &prepared.path_in_repo(Path::new(rel)))
            .map_err(err)
    };
    // Recorded paths by lowercase spelling: links match ASCII case-insensitively, and what goes
    // out keeps the recorded spelling.
    let canon: BTreeMap<String, &String> = recorded
        .iter()
        .map(|p| (p.to_ascii_lowercase(), p))
        .collect();
    // A resolved path that names a folder (`.`, `sub/`) is neither a file nor a note: it is a
    // directory on disk or a prefix of recorded paths.
    let is_folder = |rel: &str| {
        let rel = rel.trim_end_matches('/');
        let mut path = root.to_path_buf();
        path.extend(rel.split('/'));
        let prefix = format!("{}/", rel.to_ascii_lowercase());
        path.is_dir() || canon.keys().any(|p| p.starts_with(&prefix))
    };

    let chosen_notes: Vec<&String> = recorded
        .iter()
        .filter(|p| is_note_path(p) && publish_scope::is_included(p, &choices))
        .collect();
    let mut assets = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut linked_notes = BTreeSet::new();
    let mut withheld = BTreeSet::new();
    let mut too_large = BTreeSet::new();
    let over_cap_on_disk = |rel: &str| {
        let mut path = root.to_path_buf();
        path.extend(rel.split('/'));
        std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() > cap)
    };
    for note in &chosen_notes {
        let Some(body) = read(note)? else { continue };
        out.files.push(((*note).clone(), body.clone()));
        out.notes.push((*note).clone());
        for target in note_refs::targets(&String::from_utf8_lossy(&body)) {
            let resolved = match target {
                Target::Path(t) => note_refs::resolve(note, &t),
                Target::Wiki(name) => {
                    let key = if name.contains('.') {
                        name.to_ascii_lowercase()
                    } else {
                        format!("{}.md", name.to_ascii_lowercase())
                    };
                    // A name with a folder part names that folder: the recorded path is it, or
                    // ends in it after a `/`.
                    let fits = |p: &&String| {
                        let p = p.to_ascii_lowercase();
                        !key.contains('/') || p == key || p.ends_with(&format!("/{key}"))
                    };
                    let found: Vec<&String> = by_name
                        .get(file_name(&key))
                        .map_or(Vec::new(), |c| c.iter().filter(fits).collect());
                    (found.len() == 1).then(|| found[0].clone())
                }
            };
            let Some(rel) = resolved else { continue };
            if hidden(&rel) || is_folder(&rel) {
                continue;
            }
            let found = canon.get(&rel.to_ascii_lowercase()).map(|p| (*p).clone());
            if is_note_path(&rel) {
                if let Some(rec) = found {
                    if !publish_scope::is_included(&rec, &choices) {
                        linked_notes.insert(rec);
                    }
                }
            } else if let Some(rec) = found {
                // The nearest explicit choice wins; with none, the file follows the note.
                if publish_scope::nearest(&rec, &choices).is_some_and(|c| !c.include) {
                    withheld.insert(rec);
                } else {
                    assets.insert(rec);
                }
            } else if over_cap_on_disk(&rel) {
                // Not recorded because it is too large to be: saying it is missing would send
                // the person to add a version that would leave it out again.
                too_large.insert(rel);
            } else {
                missing.insert(rel);
            }
        }
    }
    for asset in assets {
        let Some(bytes) = read(&asset)? else { continue };
        // Recorded before there was a limit: it stays out all the same.
        if bytes.len() as u64 > cap {
            too_large.insert(asset);
            continue;
        }
        out.files.push((asset.clone(), bytes));
        out.assets.push(asset);
    }
    out.files.sort_by(|a, b| a.0.cmp(&b.0));
    out.missing = missing.into_iter().collect();
    out.unpublished_links = linked_notes.into_iter().collect();
    out.withheld = withheld.into_iter().collect();
    out.too_large = too_large.into_iter().collect();
    out.never_recorded = notes_on_disk(root)?
        .into_iter()
        .filter(|n| {
            !hidden(n)
                && publish_scope::is_included(n, &choices)
                && !canon.contains_key(&n.to_ascii_lowercase())
        })
        .collect();
    Ok(out)
}

/// Where the state last published from this machine is kept. It is never sent to a remote.
pub const PUBLISHED_REF: &str = "refs/textree/published";

/// Writes the assembled files under `dir`, creating folders as needed. A path that is not made of
/// plain names (one read from a history someone else wrote could climb out of `dir`) is skipped.
pub fn write_out(assembly: &Assembly, dir: &Path) -> std::io::Result<()> {
    for (rel, bytes) in &assembly.files {
        if !rel.split('/').all(crate::pathsafe::is_valid_name) {
            log::warn!("write_out: skipped a path that is not made of plain names: {rel:?}");
            continue;
        }
        let mut path = dir.to_path_buf();
        path.extend(rel.split('/'));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(())
}

/// Records what was published as a revision whose only parent is the previous publish — so the
/// published history never reaches back into the notes' history.
pub fn record_published(prepared: &VaultRepo, assembly: &Assembly) -> Result<(), String> {
    let repo = prepared.repo();
    let err = |e: git2::Error| e.message().to_string();
    let mut tree: Option<git2::Oid> = None;
    for (rel, bytes) in &assembly.files {
        let blob = repo.blob(bytes).map_err(err)?;
        let base = match tree {
            Some(id) => Some(repo.find_tree(id).map_err(err)?),
            None => None,
        };
        tree = Some(
            git_engine::tree_with_file(repo, base.as_ref(), Path::new(rel), Some(blob))
                .map_err(err)?,
        );
    }
    let Some(tree) = tree else { return Ok(()) };
    let (author, committer) = git_engine::commit_identities(repo).map_err(err)?;
    git_engine::commit_tree_on(repo, PUBLISHED_REF, tree, "Published", &author, &committer)
        .map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn record(root: &Path, rel: &str, body: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        let prepared = crate::git_engine::prepare(root).unwrap();
        let (author, committer) = crate::git_engine::commit_identities(prepared.repo()).unwrap();
        crate::git_engine::commit_paths(
            prepared.repo(),
            crate::git_engine::NOTES_REF,
            &[(prepared.path_in_repo(Path::new(rel)), body.to_vec())],
            "test",
            &author,
            &committer,
            crate::git_engine::WhenUnchanged::Skip,
        )
        .unwrap();
    }

    fn choose(root: &Path, rel: &str, include: bool) {
        let prepared = crate::git_engine::prepare(root).unwrap();
        let in_repo = crate::git_engine::slashed(&prepared.path_in_repo(Path::new(rel)));
        crate::publish_scope::set(
            prepared.repo(),
            in_repo.trim_end_matches('/'),
            Some(include),
        )
        .unwrap();
    }

    fn assembled(root: &Path) -> Assembly {
        assemble(&crate::git_engine::prepare(root).unwrap(), root).unwrap()
    }

    fn paths(a: &Assembly) -> Vec<&str> {
        a.files.iter().map(|(p, _)| p.as_str()).collect()
    }

    #[test]
    fn nothing_chosen_sends_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "a.md", b"# A");
        let a = assembled(tmp.path());
        assert!(a.nothing_chosen);
        assert!(a.files.is_empty());
    }

    #[test]
    fn gate_l2_8_a_note_not_chosen_never_goes_out() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "blog/post.md", b"# Post");
        record(tmp.path(), "private/diary.md", b"# Diary");
        record(tmp.path(), "blog/draft.md", b"# Draft");
        choose(tmp.path(), "blog", true);
        choose(tmp.path(), "blog/draft.md", false);
        assert_eq!(paths(&assembled(tmp.path())), vec!["blog/post.md"]);
    }

    #[test]
    fn gate_l2_5_an_edit_not_added_as_a_version_never_goes_out() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "a.md", b"recorded");
        std::fs::write(tmp.path().join("a.md"), b"typed later, never added").unwrap();
        choose(tmp.path(), "", true);
        let a = assembled(tmp.path());
        assert_eq!(a.files, vec![("a.md".to_string(), b"recorded".to_vec())]);
    }

    #[test]
    fn a_public_note_never_recorded_stays_out() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "a.md", b"# A");
        std::fs::write(tmp.path().join("fresh.md"), b"# Fresh").unwrap();
        choose(tmp.path(), "", true);
        let a = assembled(tmp.path());
        assert_eq!(a.notes, vec!["a.md"]);
        assert_eq!(a.never_recorded, vec!["fresh.md"]);
    }

    #[test]
    #[cfg(windows)] // a folder that does not tell names apart by case
    fn a_note_recorded_under_another_case_is_not_named_as_never_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "Note.md", b"# Note");
        // Renamed outside the app, only the case changed (the history still spells it the old way).
        std::fs::rename(tmp.path().join("Note.md"), tmp.path().join("tmp-name")).unwrap();
        std::fs::rename(tmp.path().join("tmp-name"), tmp.path().join("note.md")).unwrap();
        choose(tmp.path(), "", true);
        let a = assembled(tmp.path());
        assert!(a.never_recorded.is_empty(), "{:?}", a.never_recorded);
    }

    #[test]
    fn only_linked_files_go_out_and_missing_ones_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "img/chart.png", b"png");
        record(tmp.path(), "img/unused.png", b"png");
        record(tmp.path(), "secret.txt", b"key");
        record(
            tmp.path(),
            "blog/post.md",
            b"![c](../img/chart.png) ![g](gone.png) ![[photo.jpg]] [[Diary]]",
        );
        record(tmp.path(), "photos/photo.jpg", b"jpg");
        record(tmp.path(), "diary.md", b"# Diary");
        choose(tmp.path(), "blog", true);
        let a = assembled(tmp.path());
        assert_eq!(
            paths(&a),
            vec!["blog/post.md", "img/chart.png", "photos/photo.jpg"]
        );
        assert_eq!(a.missing, vec!["blog/gone.png"]);
        assert_eq!(a.unpublished_links, vec!["diary.md"]);
    }

    #[test]
    fn gate_l2_8_a_wiki_link_with_a_folder_part_matches_only_that_folder() {
        let shipped = |recorded: &str, link: &str| {
            let tmp = tempfile::tempdir().unwrap();
            record(tmp.path(), recorded, b"jpg");
            record(tmp.path(), "a.md", format!("![[{link}]]").as_bytes());
            choose(tmp.path(), "a.md", true);
            paths(&assembled(tmp.path())).contains(&recorded)
        };
        assert!(
            !shipped("private/photo.jpg", "public/photo.jpg"),
            "another folder"
        );
        assert!(
            !shipped("notpublic/photo.jpg", "public/photo.jpg"),
            "a folder that only ends the same"
        );
        assert!(
            shipped("public/photo.jpg", "Public/Photo.jpg"),
            "the folder named"
        );
        assert!(
            shipped("x/public/photo.jpg", "public/photo.jpg"),
            "the folder named, deeper down"
        );
        assert!(
            shipped("deep/photo.jpg", "photo.jpg"),
            "no folder part: the only one of that name"
        );
    }

    #[test]
    fn a_linked_file_over_the_limit_is_named_too_large_and_stays_out() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "old.png", b"0123456789"); // recorded before there was a limit
        std::fs::write(tmp.path().join("big.png"), b"0123456789").unwrap(); // never recorded
        std::fs::write(tmp.path().join("small.png"), b"01").unwrap(); // never recorded
        record(
            tmp.path(),
            "a.md",
            b"![](old.png) ![](big.png) ![](small.png) [d](diary.md)",
        );
        record(tmp.path(), "diary.md", b"# Diary");
        choose(tmp.path(), "a.md", true);
        let prepared = crate::git_engine::prepare(tmp.path()).unwrap();
        let a = assemble_capped(&prepared, tmp.path(), 4).unwrap();
        assert_eq!(paths(&a), vec!["a.md"]);
        assert!(a.assets.is_empty());
        assert_eq!(a.too_large, vec!["big.png", "old.png"]);
        assert_eq!(a.missing, vec!["small.png"]);
        assert_eq!(a.unpublished_links, vec!["diary.md"]);
    }

    #[test]
    fn a_deleted_note_never_goes_out() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "a.md", b"# A");
        record(tmp.path(), "b.md", b"# B ![](pic.png)");
        record(tmp.path(), "pic.png", b"png");
        choose(tmp.path(), "", true);
        std::fs::remove_file(tmp.path().join("a.md")).unwrap();
        std::fs::remove_file(tmp.path().join("pic.png")).unwrap();
        let a = assembled(tmp.path());
        assert_eq!(paths(&a), vec!["b.md"]);
        assert_eq!(a.missing, vec!["pic.png"]);
    }

    #[test]
    fn dot_files_never_go_out_even_when_linked() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), ".env", b"KEY=1");
        record(tmp.path(), "a.md", b"[x](.env)");
        choose(tmp.path(), "", true);
        assert_eq!(paths(&assembled(tmp.path())), vec!["a.md"]);
    }

    #[test]
    fn a_folder_inside_a_bigger_repository_sends_its_own_notes_only() {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let vault = tmp.path().join("docs");
        std::fs::create_dir_all(&vault).unwrap();
        record(&vault, "a.md", b"# A");
        record(tmp.path(), "README.md", b"# outside");
        choose(&vault, "", true);
        assert_eq!(paths(&assembled(&vault)), vec!["a.md"]);
    }

    #[test]
    fn a_link_to_an_empty_or_unrecorded_folder_is_not_listed_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "a.md", b"[e](empty/) [u](unrec/) [e2](empty)");
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
        std::fs::create_dir_all(tmp.path().join("unrec")).unwrap();
        std::fs::write(tmp.path().join("unrec/x.txt"), b"x").unwrap();
        choose(tmp.path(), "a.md", true);
        let a = assembled(tmp.path());
        assert_eq!(paths(&a), vec!["a.md"]);
        assert!(a.missing.is_empty(), "{:?}", a.missing);
        assert!(a.unpublished_links.is_empty() && a.assets.is_empty() && a.withheld.is_empty());
    }

    #[test]
    fn links_match_case_insensitively_and_ship_the_recorded_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "img/chart.png", b"png");
        record(tmp.path(), "Other.md", b"# Other");
        record(tmp.path(), "a.md", b"![](Img/Chart.PNG) [o](other.md)");
        choose(tmp.path(), "a.md", true);
        let a = assembled(tmp.path());
        assert_eq!(paths(&a), vec!["a.md", "img/chart.png"]);
        assert!(a.missing.is_empty(), "{:?}", a.missing);
        assert_eq!(a.unpublished_links, vec!["Other.md"]);
    }

    #[test]
    fn a_linked_file_explicitly_left_out_is_withheld() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "private/photo.png", b"png");
        record(tmp.path(), "shared/img.png", b"png");
        record(
            tmp.path(),
            "a.md",
            b"![](private/photo.png) ![](shared/img.png)",
        );
        choose(tmp.path(), "a.md", true);
        choose(tmp.path(), "private", false);
        let a = assembled(tmp.path());
        assert_eq!(paths(&a), vec!["a.md", "shared/img.png"]);
        assert_eq!(a.withheld, vec!["private/photo.png"]);
    }

    #[test]
    fn gate_l2_9_the_published_state_shares_no_history_with_the_notes() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "a.md", b"# A");
        choose(tmp.path(), "", true);
        let prepared = crate::git_engine::prepare(tmp.path()).unwrap();
        record_published(&prepared, &assembled(tmp.path())).unwrap();
        record(tmp.path(), "a.md", b"# A again");
        record_published(&prepared, &assembled(tmp.path())).unwrap();
        let repo = prepared.repo();
        let published = repo.refname_to_id(PUBLISHED_REF).unwrap();
        let notes = repo.refname_to_id(crate::git_engine::NOTES_REF).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push(published).unwrap();
        let ours: Vec<git2::Oid> = walk.map(Result::unwrap).collect();
        assert_eq!(
            ours.len(),
            2,
            "first publish has no parent, the second only the first"
        );
        let mut notes_walk = repo.revwalk().unwrap();
        notes_walk.push(notes).unwrap();
        for id in notes_walk.map(Result::unwrap) {
            assert!(!ours.contains(&id));
        }
        assert!(repo.merge_base(published, notes).is_err());
    }

    #[test]
    fn written_out_the_site_holds_exactly_what_was_assembled() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "blog/post.md", b"![c](img.png)");
        record(tmp.path(), "blog/img.png", b"png");
        record(tmp.path(), "private.md", b"# no");
        choose(tmp.path(), "blog", true);
        let out = tempfile::tempdir().unwrap();
        write_out(&assembled(tmp.path()), out.path()).unwrap();
        assert_eq!(
            std::fs::read(out.path().join("blog/post.md")).unwrap(),
            b"![c](img.png)"
        );
        assert_eq!(
            std::fs::read(out.path().join("blog/img.png")).unwrap(),
            b"png"
        );
        assert!(!out.path().join("private.md").exists());
    }

    #[test]
    fn writing_out_skips_a_path_that_is_not_made_of_plain_names() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("inner").join("site");
        let crafted = [
            "../evil.md",
            "a/../../evil2.md",
            "C:/x.md",
            "a\\..\\..\\evil3.md",
            "",
            "/abs.md",
            "a//b.md",
            "./a.md",
        ];
        let mut files: Vec<(String, Vec<u8>)> = crafted
            .iter()
            .map(|p| (p.to_string(), b"x".to_vec()))
            .collect();
        files.push(("ok/fine.md".into(), b"fine".to_vec()));
        write_out(
            &Assembly {
                files,
                ..Assembly::default()
            },
            &out,
        )
        .unwrap();
        assert_eq!(std::fs::read(out.join("ok/fine.md")).unwrap(), b"fine");
        let mut written = Vec::new();
        let mut stack = vec![tmp.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    written.push(entry.path());
                }
            }
        }
        assert_eq!(written, vec![out.join("ok").join("fine.md")]);
    }

    #[test]
    fn a_link_to_a_folder_is_not_listed_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        record(tmp.path(), "sub/inner.md", b"# Inner");
        record(tmp.path(), "a.md", b"[x](./) [y](sub/) [z](.)");
        choose(tmp.path(), "a.md", true);
        let a = assembled(tmp.path());
        assert_eq!(paths(&a), vec!["a.md"]);
        assert!(a.missing.is_empty());
        assert!(a.unpublished_links.is_empty());
        assert!(a.assets.is_empty());
    }
}

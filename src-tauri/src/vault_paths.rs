//! The one boundary between a vault and the paths that cross the IPC: a note or folder arrives as a
//! path relative to the vault (`/`-separated) and is resolved here.
//!
//! Containment is decided by the vault's [`Root`], opened once per vault: every part of the path
//! must be a plain name, and nothing on the way may lead out through a link. The path handed back
//! is built on the vault path exactly as the caller names it, part by part, so it is the same
//! string the tree hands out for that note — the per-note turns are keyed by it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri_kit_fs::{is_outside, Root};

/// What a refusal says. The frontend and the logs read this exact text.
pub const OUTSIDE: &str = "path is outside the vault";

/// Vaults opened so far, by the path the caller names them with. Opening canonicalizes, so it
/// happens once per vault rather than on every command.
fn opened(root: &str) -> Result<Root, String> {
    static ROOTS: OnceLock<Mutex<HashMap<String, Root>>> = OnceLock::new();
    let roots = ROOTS.get_or_init(Default::default);
    if let Some(r) = roots.lock().unwrap_or_else(|e| e.into_inner()).get(root) {
        // A vault removed and made again at the same path is a different folder: open it afresh.
        if r.path().is_dir() {
            return Ok(r.clone());
        }
    }
    let r = Root::open(root).map_err(|e| e.to_string())?;
    roots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root.to_string(), r.clone());
    Ok(r)
}

fn mapped(e: std::io::Error) -> String {
    if is_outside(&e) {
        OUTSIDE.into()
    } else {
        e.to_string()
    }
}

/// The parts of `rel`, refusing anything but plain names. The vault itself is never a note.
fn parts(rel: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = rel.split('/').collect();
    if rel.is_empty()
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['\\', ':']))
    {
        return Err(OUTSIDE.into());
    }
    Ok(parts)
}

/// Whether `rel` is made of plain names, from its text alone. For the one place that must not look
/// at the disk: keeping an edit for a folder that stopped answering would hang on that folder.
pub fn plain(rel: &str) -> Result<(), String> {
    parts(rel).map(|_| ())
}

fn joined(root: &str, parts: &[&str]) -> PathBuf {
    parts
        .iter()
        .fold(PathBuf::from(root), |p, name| p.join(name))
}

/// Where `rel` is, when it must already be there or its nearest existing folder must be inside.
pub fn resolve(root: &str, rel: &str) -> Result<PathBuf, String> {
    let parts = parts(rel)?;
    opened(root)?.resolve(parts.join("/")).map_err(mapped)?;
    Ok(joined(root, &parts))
}

/// A folder something is made in or moved to: `rel`, or the vault itself when `rel` is empty.
pub fn folder(root: &str, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        Ok(PathBuf::from(root))
    } else {
        resolve(root, rel)
    }
}

/// The vault-relative path of a path this side built on the vault (an operation handed it back, or
/// a command resolved it), or a refusal when it is not inside — made of plain names, as [`resolve`]
/// takes them.
pub fn rel_out(root: impl AsRef<Path>, abs: impl AsRef<Path>) -> Result<String, String> {
    let rel = rel_of(&root.as_ref().to_string_lossy(), abs.as_ref())
        .ok_or_else(|| OUTSIDE.to_string())?;
    parts(&rel)?;
    Ok(rel)
}

/// The vault-relative path of `abs`, a path this side built on the same vault path.
pub fn rel_of(root: &str, abs: &Path) -> Option<String> {
    let rel = |rest: &Path| Some(crate::git_engine::slashed(rest)).filter(|s| !s.is_empty());
    if let Ok(rest) = abs.strip_prefix(root) {
        return rel(rest);
    }
    // Spelled differently (case, a short name, a trailing separator): compare where both really are.
    let base = opened(root).ok()?;
    let canonical = dunce_like(abs)?;
    rel(canonical.strip_prefix(base.path()).ok()?)
}

/// `abs` canonicalized, or — when it no longer exists — its parent canonicalized with the name
/// put back.
fn dunce_like(abs: &Path) -> Option<PathBuf> {
    match std::fs::canonicalize(abs) {
        Ok(p) => Some(strip_verbatim(p)),
        Err(_) => {
            let parent = std::fs::canonicalize(abs.parent()?).ok()?;
            Some(strip_verbatim(parent).join(abs.file_name()?))
        }
    }
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        (d, root)
    }

    #[test]
    fn resolve_and_rel_of_round_trip() {
        let (d, root) = vault();
        std::fs::create_dir_all(d.path().join("a").join("b")).unwrap();
        std::fs::write(d.path().join("a").join("b").join("n.md"), "x").unwrap();
        let abs = resolve(&root, "a/b/n.md").unwrap();
        assert_eq!(
            abs,
            d.path().join("a").join("b").join("n.md"),
            "built part by part on the vault path"
        );
        assert_eq!(rel_of(&root, &abs).as_deref(), Some("a/b/n.md"));
    }

    #[test]
    fn a_path_that_does_not_exist_yet_resolves_when_its_folder_is_inside() {
        let (d, root) = vault();
        assert_eq!(
            resolve(&root, "new/n.md").unwrap(),
            d.path().join("new").join("n.md")
        );
    }

    #[test]
    fn plain_names_are_judged_from_the_text_alone() {
        assert_eq!(plain("sub/a.md"), Ok(()));
        for bad in ["", "../escape.md", "a/../../b.md", "C:/x.md"] {
            assert_eq!(plain(bad).unwrap_err(), OUTSIDE, "{bad:?}");
        }
    }

    #[test]
    fn anything_but_plain_names_is_outside() {
        let (_d, root) = vault();
        for bad in [
            "",
            "../x.md",
            "a/../../x.md",
            "./x.md",
            "a//b.md",
            "C:/Windows/x.md",
            "/etc/x",
            "a\\..\\..\\x.md",
        ] {
            assert_eq!(resolve(&root, bad).unwrap_err(), OUTSIDE, "{bad:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_link_out_of_the_vault_is_outside() {
        let (vault, root) = vault();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("s.md"), "x").unwrap();
        let link = vault.path().join("j");
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(resolve(&root, "j/s.md").unwrap_err(), OUTSIDE);
        assert_eq!(resolve(&root, "j/new.md").unwrap_err(), OUTSIDE);
    }

    #[test]
    fn rel_of_a_path_under_a_differently_spelled_vault() {
        let (d, _) = vault();
        std::fs::write(d.path().join("n.md"), "x").unwrap();
        let typed = format!(
            "{}{}",
            d.path().to_string_lossy(),
            std::path::MAIN_SEPARATOR
        );
        let canonical = strip_verbatim(std::fs::canonicalize(d.path()).unwrap()).join("n.md");
        assert_eq!(rel_of(&typed, &canonical).as_deref(), Some("n.md"));
        // A note already gone still has a place.
        let gone = strip_verbatim(std::fs::canonicalize(d.path()).unwrap()).join("gone.md");
        assert_eq!(rel_of(&typed, &gone).as_deref(), Some("gone.md"));
    }

    /// The per-note turns are keyed by path: a command that resolves here and one that is handed a
    /// path from the tree must name a note with the very same string.
    #[test]
    fn a_resolved_path_is_the_one_the_tree_hands_out() {
        let (d, _) = vault();
        std::fs::create_dir_all(d.path().join("blog")).unwrap();
        std::fs::write(d.path().join("blog").join("post.md"), "x").unwrap();
        for root in [
            d.path().to_string_lossy().to_string(),
            d.path().to_string_lossy().replace('\\', "/"),
        ] {
            let tree = crate::vault::build_tree(Path::new(&root)).unwrap();
            let post = &tree[0].children[0].path;
            assert_eq!(
                resolve(&root, "blog/post.md").unwrap().as_os_str(),
                post.as_os_str(),
                "{root}"
            );
        }
    }

    #[test]
    fn the_vault_itself_and_paths_elsewhere_have_no_rel() {
        let (d, root) = vault();
        let other = tempfile::tempdir().unwrap();
        assert_eq!(rel_of(&root, d.path()), None);
        assert_eq!(rel_of(&root, &other.path().join("x.md")), None);
    }
}

//! The sidebar's notes folders as this device lists them: adding, removing, ordering, naming, and
//! pointing one at where it now is — with the settings and the remote secret kept for it.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri_kit_state::folder_key;

use crate::commands::{personal_dir, personal_home, NOT_FOUND};

/// The sidebar's notes folders, as the screen reads them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOut {
    pub roots: Vec<crate::workspace_file::Root>,
    /// Changes are not kept: the list was written by a newer release.
    pub read_only: bool,
    /// Where an unreadable list was moved this launch.
    pub set_aside: Option<String>,
}

fn workspace_out(ws: &crate::workspace_file::Workspace) -> WorkspaceOut {
    remember_kept_from_ai(ws.roots());
    WorkspaceOut {
        roots: ws.roots().to_vec(),
        read_only: !ws.writable(),
        set_aside: ws.set_aside().map(|p| p.display().to_string()),
    }
}

/// The sidebar's notes folders as kept on this device. Each one is registered, so commands can
/// name it by its id before it is opened.
pub fn workspace_list() -> Result<WorkspaceOut, String> {
    let _turn = list_turn();
    let ws = crate::workspace_file::Workspace::load(&crate::workspace_file::dir_under(
        &personal_home()?,
    ))
    .map_err(|e| e.to_string())?;
    for root in ws.roots() {
        crate::workspace::register(&root.path);
    }
    Ok(workspace_out(&ws))
}

/// Keeps the notes folder at `path` in the list — every folder the window opens, so the first
/// launch with a list takes the folder an earlier release opened last.
pub fn workspace_add(path: String) -> Result<WorkspaceOut, String> {
    let _turn = list_turn();
    let mut ws = crate::workspace_file::Workspace::load(&crate::workspace_file::dir_under(
        &personal_home()?,
    ))
    .map_err(|e| e.to_string())?;
    ws.add(&path).map_err(|e| e.to_string())?;
    for root in ws.roots() {
        crate::workspace::register(&root.path);
    }
    Ok(workspace_out(&ws))
}

/// Held from reading the device's list to writing it back. Commands run off the main thread and
/// may overlap — at startup each folder opened adds itself while the person may already be
/// adding or naming one — and two that read the same list would each write back their own change
/// without the other's.
fn list_turn() -> std::sync::MutexGuard<'static, ()> {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TURN.lock().unwrap_or_else(|p| p.into_inner())
}

/// Changes the sidebar's list of notes folders with `change` and returns the list as it now is.
fn workspace_change(
    change: impl FnOnce(&mut crate::workspace_file::Workspace) -> std::io::Result<()>,
) -> Result<WorkspaceOut, String> {
    let _turn = list_turn();
    let mut ws = crate::workspace_file::Workspace::load(&crate::workspace_file::dir_under(
        &personal_home()?,
    ))
    .map_err(|e| e.to_string())?;
    change(&mut ws).map_err(|e| e.to_string())?;
    Ok(workspace_out(&ws))
}

/// Takes a notes folder out of the sidebar. The folder and its notes stay where they are.
pub fn workspace_remove(id: String) -> Result<WorkspaceOut, String> {
    workspace_change(|ws| ws.remove(&id).map(drop))
}

/// Puts the sidebar's notes folders in the order `ids` names them.
pub fn workspace_reorder(ids: Vec<String>) -> Result<WorkspaceOut, String> {
    workspace_change(|ws| ws.reorder(&ids))
}

/// Turns the use of a notes folder with AI on or off. Off, the folder is not indexed for AI, not
/// searched by meaning and not asked about — decided here, at the app's edge, so the AI host never
/// hears of the folder.
pub fn workspace_set_ai(id: String, ai: bool) -> Result<WorkspaceOut, String> {
    workspace_change(|ws| ws.set_ai(&id, ai).map(drop))
}

/// The folders kept from AI, as the device's list last said (by folder identity).
static KEPT_FROM_AI: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

fn remember_kept_from_ai(roots: &[crate::workspace_file::Root]) {
    let kept = roots
        .iter()
        .filter(|r| !r.ai)
        .map(|r| crate::search::vault_identity(Path::new(&r.path)))
        .collect();
    *KEPT_FROM_AI.lock().unwrap_or_else(|p| p.into_inner()) = kept;
}

/// Whether the notes folder at `vault` may be used with AI. A folder the device's list does not
/// hold, or holds without saying, is.
pub fn used_with_ai(vault: &str) -> bool {
    !KEPT_FROM_AI
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(&crate::search::vault_identity(Path::new(vault)))
}

/// Names a notes folder in the sidebar; an empty name goes back to the folder's own.
pub fn workspace_relabel(id: String, label: String) -> Result<WorkspaceOut, String> {
    workspace_change(|ws| ws.relabel(&id, &label).map(drop))
}

/// What became of a moved notes folder's settings when the sidebar was pointed at its new place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingsCarried {
    /// They moved with it.
    Moved,
    /// There were none to move.
    None,
    /// The new place already had settings of its own: those are used, and the old ones are left
    /// where they were — nothing is overwritten.
    KeptApart,
}

/// The sidebar's list after a notes folder was pointed at its new place.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Relocated {
    pub workspace: WorkspaceOut,
    /// The folder's id now — the id of its new path.
    pub id: String,
    pub settings: SettingsCarried,
}

/// Points the notes folder `id` at `path`, where it now is («Locate…»): it keeps its place and
/// name in the sidebar, and its settings — favorites, order, views, its remote and the secret for
/// it, edits kept from notes it left, drafts of alternatives — move with it. The settings move
/// first and the list is written last, so a move cut short is finished by doing it again.
pub fn workspace_relocate(id: String, path: String) -> Result<Relocated, String> {
    let _turn = list_turn();
    let mut ws = crate::workspace_file::Workspace::load(&crate::workspace_file::dir_under(
        &personal_home()?,
    ))
    .map_err(|e| e.to_string())?;
    if !ws.writable() {
        // Its list could not be changed, so its settings must not move either.
        return Err(
            "The list of notes folders is from a newer release of Textree. \
                    Update Textree to change it."
                .into(),
        );
    }
    let from = ws
        .roots()
        .iter()
        .find(|r| r.id == id)
        .map(|r| PathBuf::from(&r.path))
        .ok_or_else(|| "This notes folder is no longer in the sidebar.".to_string())?;
    let to = Path::new(&path);
    if !to.is_dir() {
        return Err(NOT_FOUND.into());
    }
    ws.check_relocation(&id, &path).map_err(|e| e.to_string())?;
    let settings = carry_settings(&from, to)?;
    carry_secret(&from, to)?;
    let root = ws.relocate(&id, &path).map_err(|e| e.to_string())?;
    crate::workspace::register(&root.path);
    Ok(Relocated {
        workspace: workspace_out(&ws),
        id: root.id,
        settings,
    })
}

/// The keys the settings of the folder that was at `path` may be kept under, most likely first.
///
/// A folder's key is made from its resolved path ([`folder_key`]), and a folder that is gone can no
/// longer be resolved: keyed as written, it would miss its settings wherever the written path is
/// not the resolved one (a link on the way, a `\\?\` prefix). So the part of the path that is
/// still there is resolved and the rest joined to it — what resolving gave while the folder was
/// there, unless a link inside the part that is gone. The path as written, and on Windows its
/// verbatim form (a whole drive gone), follow.
fn keys_of(path: &Path) -> Vec<String> {
    let mut keys = vec![
        folder_key(&resolved_as_far_as_there(path)),
        folder_key(path),
    ];
    #[cfg(windows)]
    {
        let written = path.to_string_lossy();
        let verbatim = match written.strip_prefix(r"\\") {
            Some(unc) if !unc.starts_with(r"?\") => format!(r"\\?\UNC\{unc}"),
            Some(_) => written.into_owned(),
            None => format!(r"\\?\{written}"),
        };
        keys.push(folder_key(Path::new(&verbatim)));
    }
    let mut seen = std::collections::HashSet::new();
    keys.retain(|k| seen.insert(k.clone()));
    keys
}

/// `path` resolved as far as it exists, with the rest joined as written.
fn resolved_as_far_as_there(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = at.canonicalize() {
            return rest.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Moves the settings of the folder that was at `from` to where `to`'s are kept — never over
/// settings already there.
fn carry_settings(from: &Path, to: &Path) -> Result<SettingsCarried, String> {
    let there = personal_dir(to)?;
    let home = personal_dir(Path::new(""))?;
    let base = home.parent().ok_or("cannot find where settings are kept")?;
    let Some(here) = keys_of(from)
        .into_iter()
        .map(|key| base.join(key))
        .find(|dir| dir.is_dir())
    else {
        return Ok(SettingsCarried::None);
    };
    if here == there {
        return Ok(SettingsCarried::Moved); // the same folder spelled another way
    }
    if there.exists() {
        return Ok(SettingsCarried::KeptApart);
    }
    std::fs::create_dir_all(base).map_err(|e| e.to_string())?;
    match crate::fs_ops::rename(&here, &there) {
        Ok(()) => Ok(SettingsCarried::Moved),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(SettingsCarried::KeptApart),
        Err(e) => Err(e.to_string()),
    }
}

/// Moves the secret kept for the remote of the folder that was at `from` to `to` — written under
/// the new key before it is forgotten under the old, and never over one the new place has.
fn carry_secret(from: &Path, to: &Path) -> Result<(), String> {
    let new = folder_key(to);
    if crate::remote::secret(&new).is_some() {
        return Ok(());
    }
    for old in keys_of(from).into_iter().filter(|k| *k != new) {
        if let Some(secret) = crate::remote::secret(&old) {
            crate::remote::set_secret(&new, &secret)?;
            return crate::remote::clear_secret(&old);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_kept_from_ai_is_refused_at_the_edge_however_its_path_is_spelled() {
        // Every command that rewrites the remembered set holds this turn; so does the test.
        let _turn = list_turn();
        let d = tempfile::tempdir().unwrap();
        let kept = d.path().join("kept");
        let used = d.path().join("used");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::create_dir_all(&used).unwrap();
        let root = |p: &std::path::Path, ai| crate::workspace_file::Root {
            id: crate::workspace::id_for(&p.to_string_lossy()),
            path: p.to_string_lossy().into_owned(),
            label: None,
            ai,
        };
        remember_kept_from_ai(&[root(&kept, false), root(&used, true)]);
        assert!(!used_with_ai(&kept.to_string_lossy()));
        assert!(
            !used_with_ai(&kept.join(".").to_string_lossy()),
            "another spelling of the same folder"
        );
        assert!(used_with_ai(&used.to_string_lossy()));
        assert!(used_with_ai(&d.path().join("unlisted").to_string_lossy()));
        remember_kept_from_ai(&[]);
        assert!(
            used_with_ai(&kept.to_string_lossy()),
            "turned back on, it is used again"
        );
    }
    use tempfile::TempDir;

    #[test]
    fn the_workspace_keeps_each_opened_folder_once_and_registers_them() {
        workspace_add("/notes/workspace-test-a".into()).unwrap();
        workspace_add("/notes/workspace-test-b".into()).unwrap();
        let listed = workspace_add("/notes/workspace-test-a".into()).unwrap();
        let paths: Vec<_> = listed
            .roots
            .iter()
            .map(|r| r.path.as_str())
            .filter(|p| p.starts_with("/notes/workspace-test-"))
            .collect();
        assert_eq!(
            paths,
            ["/notes/workspace-test-a", "/notes/workspace-test-b"]
        );
        assert!(!listed.read_only);
        for root in workspace_list().unwrap().roots {
            assert_eq!(crate::workspace::path_of(&root.id).unwrap(), root.path);
        }
    }

    #[test]
    fn a_located_folder_brings_its_settings_and_secret_and_overwrites_none() {
        let tmp = TempDir::new().unwrap();
        // Not resolved as written on every system (a temporary folder behind a link on macOS):
        // its settings are found under the key it had while it was there.
        let (gone, moved) = (tmp.path().join("gone"), tmp.path().join("moved"));
        std::fs::create_dir(&gone).unwrap();
        let gone_s = gone.to_string_lossy().to_string();
        let old_dir = personal_dir(&gone).unwrap();
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("favorites.json"), "[\"a.md\"]").unwrap();
        crate::remote::set_secret(&folder_key(&gone), "s3cret").unwrap();
        let id = workspace_add(gone_s.clone())
            .unwrap()
            .roots
            .into_iter()
            .find(|r| r.path == gone_s)
            .unwrap()
            .id;
        std::fs::rename(&gone, &moved).unwrap();
        // On macOS the temporary folder is behind a link, so keyed as written it would be missed.
        #[cfg(target_os = "macos")]
        assert_ne!(
            old_dir.file_name().unwrap().to_string_lossy(),
            folder_key(&gone)
        );

        let done = workspace_relocate(id.clone(), moved.to_string_lossy().into()).unwrap();

        assert_eq!(done.settings, SettingsCarried::Moved);
        let new_dir = personal_dir(&moved).unwrap();
        assert_eq!(
            std::fs::read_to_string(new_dir.join("favorites.json")).unwrap(),
            "[\"a.md\"]"
        );
        assert!(!old_dir.exists());
        assert_eq!(
            crate::remote::secret(&folder_key(&moved)).as_deref(),
            Some("s3cret")
        );
        assert!(crate::remote::secret(&folder_key(&gone)).is_none());
        let listed = done
            .workspace
            .roots
            .iter()
            .find(|r| r.id == done.id)
            .unwrap();
        assert_eq!(listed.path, moved.to_string_lossy());
        assert!(done.workspace.roots.iter().all(|r| r.id != id));

        // Another folder located where settings already are: those stay, the old are not lost.
        let (other, there) = (tmp.path().join("other"), tmp.path().join("there"));
        std::fs::create_dir(&other).unwrap();
        std::fs::create_dir(&there).unwrap();
        let other_dir = personal_dir(&other).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(other_dir.join("order.json"), "old").unwrap();
        let there_dir = personal_dir(&there).unwrap();
        std::fs::create_dir_all(&there_dir).unwrap();
        std::fs::write(there_dir.join("order.json"), "new").unwrap();
        let other_s = other.to_string_lossy().to_string();
        let other_id = workspace_add(other_s.clone())
            .unwrap()
            .roots
            .into_iter()
            .find(|r| r.path == other_s)
            .unwrap()
            .id;
        std::fs::remove_dir(&other).unwrap();
        let kept = workspace_relocate(other_id, there.to_string_lossy().into()).unwrap();
        assert_eq!(kept.settings, SettingsCarried::KeptApart);
        assert_eq!(
            std::fs::read_to_string(there_dir.join("order.json")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(other_dir.join("order.json")).unwrap(),
            "old"
        );

        // Nowhere to point it: said the way opening says it.
        assert_eq!(
            workspace_relocate(kept.id, tmp.path().join("nowhere").to_string_lossy().into())
                .unwrap_err(),
            NOT_FOUND
        );
        crate::remote::clear_secret(&folder_key(&moved)).unwrap();
    }
}

//! IPC entry points for the commands that change the tree or read the folder's history — run off
//! the main thread.
//!
//! A command that is not `async` runs on the main thread, and there a folder that stops answering
//! (a stalled sync or network drive) freezes the window, as does a render that takes seconds. Each
//! entry here moves its work to a blocking thread and, when it changes the tree, first takes a
//! turn over the places it changes ([`NoteLocks::subtree`]) so no save lands inside them midway.
//!
//! The work itself lives in [`crate::commands`] under the same names, where tests call it
//! directly. Only places a change reads from need a turn: a save never creates a file (one whose
//! note is not there is refused as gone), so a place a change writes to cannot be raced by one.
//!
//! Creating does add to a folder, though, and a folder being deleted has already been kept as it
//! was: something created in it meanwhile would be deleted with it, kept nowhere. So creating takes
//! the folder's own turn ([`creating`]) — it waits while that folder, or one above it, is being
//! changed, and such a change waits for it.

use crate::commands::{
    self, off_main, DefaultVault, DeletedNote, MoveOut, NoteAlternative, NoteVersion, RestoredNote,
    SidecarState,
};
use crate::note_locks::NoteLocks;
use crate::search::IndexHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Where `rel` is in the vault, worked out off the main thread: it looks at the disk, and a folder
/// that stopped answering must hold up this request only.
async fn resolved(root: &str, rel: &str) -> Result<String, String> {
    let (root, rel) = (root.to_string(), rel.to_string());
    off_main(move || {
        crate::vault_paths::resolve(&root, &rel).map(|p| p.to_string_lossy().into_owned())
    })
    .await
}

/// Like [`resolved`], for a folder something is made in or moved to; `""` is the vault itself.
async fn folder_resolved(root: &str, rel: &str) -> Result<String, String> {
    let (root, rel) = (root.to_string(), rel.to_string());
    off_main(move || {
        crate::vault_paths::folder(&root, &rel).map(|p| p.to_string_lossy().into_owned())
    })
    .await
}

/// Opening a folder reads all of it; one that does not answer must not hold the window, or keep
/// the person from opening another instead (the later opening wins — see
/// [`commands::open_vault`]).
#[tauri::command]
pub async fn open_vault(
    root_id: String,
    app: AppHandle,
) -> Result<Vec<commands::TreeItem>, String> {
    let ticket = commands::begin_open();
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || {
        commands::open_vault(root.clone(), root_id, app, ticket)
            .and_then(|tree| commands::tree_out(&root, tree))
    })
    .await
}

/// Makes the folder at `path` known and answers with the id every later command names it by. It
/// only records the name — nothing on the disk is looked at — so the screen can hold the id
/// before the folder has finished opening.
#[tauri::command]
pub async fn register_root(path: String) -> Result<String, String> {
    Ok(crate::workspace::register(&path))
}

/// Runs `work` off the main thread while holding a turn over `places`.
async fn changing<T: Send + 'static>(
    locks: &State<'_, Arc<NoteLocks>>,
    places: Vec<String>,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let locks = locks.inner().clone();
    off_main(move || {
        let places: Vec<&Path> = places.iter().map(Path::new).collect();
        let _turn = locks.subtree(&places);
        work()
    })
    .await
}

/// Runs `work` off the main thread while holding the turn of the folder it creates something in.
async fn creating<T: Send + 'static>(
    locks: &State<'_, Arc<NoteLocks>>,
    folder: PathBuf,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let locks = locks.inner().clone();
    off_main(move || {
        let _turn = locks.turn(&folder);
        work()
    })
    .await
}

/// The folder `path` is in — where creating something beside it adds to.
fn folder_of(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

#[tauri::command]
pub async fn rename_node(
    root_id: String,
    rel: String,
    name: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::rename_node(root.clone(), path, name)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

#[tauri::command]
pub async fn rename_note_unique(
    root_id: String,
    rel: String,
    name: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::rename_note_unique(root.clone(), path, name)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

#[tauri::command]
pub async fn move_node(
    root_id: String,
    rel: String,
    dest: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    let dest = folder_resolved(&root, &dest).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::move_node(root.clone(), path, dest)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

#[tauri::command]
pub async fn delete_node(
    root_id: String,
    rel: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::delete_node(root, path)
    })
    .await
}

/// Promoting replaces the note with a folder of the same name holding it.
#[tauri::command]
pub async fn promote_node(
    root_id: String,
    rel: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::promote_node(root.clone(), path)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

/// Adopting moves `path` under `leaf`, promoting `leaf` first — both are read from.
#[tauri::command]
pub async fn adopt_node(
    root_id: String,
    rel: String,
    leaf: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    let leaf = resolved(&root, &leaf).await?;
    let places = vec![path.clone(), leaf.clone()];
    changing(&locks, places, move || {
        commands::adopt_node(root.clone(), path, leaf)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

/// The open alternatives of the notes in a folder. Reads history only.
#[tauri::command]
pub async fn list_alternatives(root_id: String) -> Result<Vec<NoteAlternative>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::list_alternatives(root)).await
}

/// Starting an alternative writes a reference only — the note's file is not touched.
#[tauri::command]
pub async fn start_alternative(root_id: String, rel: String) -> Result<NoteAlternative, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    off_main(move || commands::start_alternative(root, path)).await
}

#[tauri::command]
pub async fn alternative_text(root_id: String, id: String) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::alternative_text(root, id)).await
}

/// What is being typed into an alternative is kept outside the folder.
#[tauri::command]
pub async fn alternative_draft(root_id: String, id: String) -> Result<Option<String>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::alternative_draft(root, id)).await
}

#[tauri::command]
pub async fn write_alternative_draft(
    root_id: String,
    id: String,
    text: String,
) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::write_alternative_draft(root, id, text)).await
}

/// Adding a version to an alternative writes a reference only.
#[tauri::command]
pub async fn add_alternative_version(
    root_id: String,
    id: String,
    text: String,
    message: String,
) -> Result<Option<String>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::add_alternative_version(root, id, text, message)).await
}

/// Using an alternative writes over the note, so it takes the note's turn like a save.
#[tauri::command]
pub async fn use_alternative(
    root_id: String,
    rel: String,
    id: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::use_alternative(root, path, id)
    })
    .await
}

/// Setting an alternative aside moves a reference only; the note stays as it is.
#[tauri::command]
pub async fn set_aside_alternative(root_id: String, id: String) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::set_aside_alternative(root, id)).await
}

/// Bringing back an earlier version writes over the note, so it takes the note's turn like a save.
#[tauri::command]
pub async fn restore_version(
    root_id: String,
    rel: String,
    id: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    changing(&locks, vec![path.clone()], move || {
        commands::restore_version(root, path, id)
    })
    .await
}

// The rest create what was not there — in the turn of the folder they add to ([`creating`]).

#[tauri::command]
pub async fn restore_deleted(
    root_id: String,
    rel: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<RestoredNote, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let folder = folder_of(&Path::new(&root).join(&rel).to_string_lossy());
    creating(&locks, folder, move || commands::restore_deleted(root, rel)).await
}

#[tauri::command]
pub async fn create_note(
    root_id: String,
    parent: String,
    name: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let parent = folder_resolved(&root, &parent).await?;
    creating(&locks, PathBuf::from(&parent), move || {
        commands::create_note(root.clone(), parent, name)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

#[tauri::command]
pub async fn create_untitled_note(
    root_id: String,
    parent: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let parent = folder_resolved(&root, &parent).await?;
    creating(&locks, PathBuf::from(&parent), move || {
        commands::create_untitled_note(root.clone(), parent)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

#[tauri::command]
pub async fn create_note_with_content(
    root_id: String,
    parent: String,
    name: String,
    content: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let parent = folder_resolved(&root, &parent).await?;
    creating(&locks, PathBuf::from(&parent), move || {
        commands::create_note_with_content(root.clone(), parent, name, content)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

#[tauri::command]
pub async fn create_folder(
    root_id: String,
    parent: String,
    name: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let parent = folder_resolved(&root, &parent).await?;
    creating(&locks, PathBuf::from(&parent), move || {
        commands::create_folder(root.clone(), parent, name)
            .and_then(|p| crate::vault_paths::rel_out(&root, &p))
    })
    .await
}

/// An attachment goes into an `assets` folder beside its note — the turn is that folder's, which
/// deleting the note's folder or `assets` itself both wait for.
#[tauri::command]
pub async fn save_attachment(
    root_id: String,
    note: String,
    data: String,
    ext: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let note = resolved(&root, &note).await?;
    let folder = folder_of(&note).join("assets");
    creating(&locks, folder, move || {
        commands::save_attachment(root, note, data, ext)
    })
    .await
}

/// Recording reads the notes and changes none of them: a save running meanwhile is recorded as it
/// was either just before or just after — each a state the note really held.
#[tauri::command]
pub async fn commit_notes(
    root_id: String,
    rels: Vec<String>,
    message: String,
) -> Result<Option<String>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || {
        let paths = rels
            .iter()
            .map(|rel| {
                crate::vault_paths::resolve(&root, rel).map(|p| p.to_string_lossy().into_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        commands::commit_notes(root, paths, message)
    })
    .await
}

#[tauri::command]
pub async fn move_state_out_of_vault(root_id: String) -> Result<MoveOut, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::move_state_out_of_vault(root)).await
}

#[tauri::command]
pub async fn note_versions(root_id: String, rel: String) -> Result<Vec<NoteVersion>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    off_main(move || commands::note_versions(root, path)).await
}

#[tauri::command]
pub async fn note_version_text(root_id: String, rel: String, id: String) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let path = resolved(&root, &rel).await?;
    off_main(move || commands::note_version_text(root, path, id)).await
}

#[tauri::command]
pub async fn deleted_notes(root_id: String) -> Result<Vec<DeletedNote>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::deleted_notes(root)).await
}

#[tauri::command]
pub async fn rebuild_index(
    root_id: String,
    index: State<'_, Arc<IndexHandle>>,
) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    let index = index.inner().clone();
    off_main(move || commands::rebuild_index(root, &index)).await
}

/// Rendering can take seconds (much longer on a cold start); the window stays responsive meanwhile.
#[tauri::command]
pub async fn publish_site(
    app: AppHandle,
    root_id: String,
    out_dir: String,
    options: crate::publish::PublishOptions,
) -> Result<crate::publish::PublishResult, String> {
    let vault_path = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::publish_site(app, vault_path, out_dir, options)).await
}

/// Walks the whole folder and reads every note — as slow as the folder is.
#[tauri::command]
pub async fn publish_preview(root_id: String) -> Result<commands::PublishPreview, String> {
    let vault_path = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::publish_preview(vault_path)).await
}

#[tauri::command]
pub async fn publish_choices(root_id: String) -> Result<Vec<crate::publish_scope::Choice>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::publish_choices(root)).await
}

#[tauri::command]
pub async fn set_publish_choice(
    root_id: String,
    rel: String,
    choice: Option<bool>,
) -> Result<Vec<crate::publish_scope::Choice>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::set_publish_choice(root, rel, choice)).await
}

#[tauri::command]
pub async fn remote_connection(
    root_id: String,
) -> Result<Option<crate::remote::Connection>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::remote_connection(root)).await
}

/// Reaching the remote is network: it must not hold the window.
#[tauri::command]
pub async fn connect_remote(
    root_id: String,
    url: String,
    username: String,
    secret: String,
) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::connect_remote(root, url, username, secret)).await
}

#[tauri::command]
pub async fn notes_backed_up(root_id: String) -> Result<bool, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::notes_backed_up(root)).await
}

#[tauri::command]
pub async fn disconnect_remote(root_id: String) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::disconnect_remote(root)).await
}

/// Takes the turns of the notes it writes itself (see [`crate::remote::apply`]).
#[tauri::command]
pub async fn sync_remote(
    root_id: String,
    locks: State<'_, Arc<NoteLocks>>,
) -> Result<crate::remote::Exchange, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let locks = locks.inner().clone();
    off_main(move || commands::sync_remote(root, &locks)).await
}

#[tauri::command]
pub async fn publish_to_cloud(
    app: AppHandle,
    root_id: String,
    options: crate::publish::PublishOptions,
) -> Result<crate::cloud_publish::PublishToCloudResult, String> {
    let vault_path = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::publish_to_cloud(app, vault_path, options)).await
}

// A folder's settings live outside it, but finding them names the folder: the key is worked out
// from its resolved path, and resolving asks the folder's drive — which may be the one that stopped
// answering.

#[tauri::command]
pub async fn read_sidecar(root_id: String, rel: String) -> Result<Option<String>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::read_sidecar(root, rel)).await
}

#[tauri::command]
pub async fn write_sidecar(root_id: String, rel: String, content: String) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::write_sidecar(root, rel, content)).await
}

#[tauri::command]
pub async fn prepare_sidecar(root_id: String) -> Result<SidecarState, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::prepare_sidecar(root)).await
}

#[tauri::command]
pub async fn set_aside_sidecar(root_id: String, rel: String) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || commands::set_aside_sidecar(root, rel)).await
}

/// The first folder is made and seeded in Documents, which may be redirected to a synced drive.
#[tauri::command]
pub async fn ensure_default_vault(app: AppHandle) -> Result<DefaultVault, String> {
    off_main(move || commands::ensure_default_vault(app)).await
}

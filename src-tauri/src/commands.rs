use crate::host;
use crate::note_locks::NoteLocks;
use crate::note_refs::is_note_path;
use crate::search::{IndexHandle, IndexState, SearchHit};
use crate::vault::{self, TreeNode};
use crate::watcher::WatcherHandle;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};
use tauri_kit_state::folder_key;
use tempfile::NamedTempFile;

/// Name of the staging directory for atomic writes, inside the repository's own storage.
pub(crate) const TEMP_DIR_NAME: &str = "textree-tmp";

/// Prefix of a temp file staged at the top of a folder no repository governs. The application's
/// own, so that one left by a crash can be recognized and removed without touching anything else.
pub(crate) const LOOSE_TEMP_PREFIX: &str = ".textree-tmp-";

/// Legacy staging location, kept only so leftovers from earlier versions get cleaned up.
const LEGACY_TEMP_DIR: [&str; 2] = [".textree", "tmp"];

/// Where atomic writes stage their temp file.
///
/// Repository storage is the right home for it: it is on the same volume as the target (so
/// `persist` is still an atomic rename), it is not part of anyone's working tree (so transient
/// `.tmpXXXX` files never show up as changes, not even when the folder sits inside a
/// repository someone else uses), and it leaves no trace in the folder itself.
///
/// A folder no repository governs has no such place, and gets none: making a directory in it
/// would be the one thing the folder is promised never to hold: anything but notes. Writes there stage
/// beside their target instead — see [`atomic_write_bytes`].
pub(crate) fn temp_dir(root: &Path) -> Option<PathBuf> {
    crate::git_engine::git_dir(root).map(|git| git.join(TEMP_DIR_NAME))
}

/// Best-effort removal of orphaned temp files — e.g. a crash or power loss between create and
/// rename. Without this they would linger forever. Called on vault open. Errors are ignored (an
/// in-flight temp held open by another instance simply stays).
///
/// The older location is swept as well, so upgrading leaves nothing behind — and so is the top of
/// the folder, for temps staged there before a repository existed. Only names carrying the
/// application's own prefix go, and only at the top: a sweep never walks the whole folder.
pub(crate) fn clear_temp_dir(root: &Path) {
    let legacy = root.join(LEGACY_TEMP_DIR[0]).join(LEGACY_TEMP_DIR[1]);
    for dir in temp_dir(root).into_iter().chain([legacy]) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let ours = entry
                .file_name()
                .to_string_lossy()
                .starts_with(LOOSE_TEMP_PREFIX);
            if ours && entry.path().is_file() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Atomic write for a file that is not inside the vault, so the temp file goes beside the
/// destination. A rename is only atomic within one volume, and settings live wherever the
/// person's home is — which is often not the volume the notes are on.
fn atomic_write_beside(path: &Path, content: &str) -> io::Result<()> {
    atomic_bytes_beside(path, content.as_bytes())
}

/// The same guarantee for content that is not necessarily text, so that carrying a file across
/// does not require being able to read it.
///
/// A refusal that passes on its own is waited out by the write itself, and each attempt leaves the
/// old content or the new.
fn atomic_bytes_beside(path: &Path, content: &[u8]) -> io::Result<()> {
    tauri_kit_fs::write_atomic(path, content)
}

/// Atomic file write: write to a temp file in repository storage (or beside the target when no
/// repository governs the folder), then rename onto the target. Even if a crash or power loss
/// happens mid-write, the target file is not truncated ("the FS is the truth").
pub(crate) fn atomic_write(root: &Path, path: &Path, content: &str) -> io::Result<()> {
    atomic_write_bytes(root, path, content.as_bytes())
}

/// The same guarantee for content that is not necessarily text — a restored file can be
/// anything that was kept alongside the notes.
pub(crate) fn atomic_write_bytes(root: &Path, path: &Path, content: &[u8]) -> io::Result<()> {
    atomic_write_in_vault(root, path, content, Landing::Replace)
}

/// [`atomic_write_bytes`] for a file that must not exist yet: refuses with
/// [`io::ErrorKind::AlreadyExists`] rather than replacing one that arrived after its place was
/// found free.
pub(crate) fn atomic_create_bytes(root: &Path, path: &Path, content: &[u8]) -> io::Result<()> {
    atomic_write_in_vault(root, path, content, Landing::New)
}

/// Whether an atomic write may replace what is at its target.
#[derive(Clone, Copy)]
enum Landing {
    Replace,
    New,
}

fn atomic_write_in_vault(
    root: &Path,
    path: &Path,
    content: &[u8],
    landing: Landing,
) -> io::Result<()> {
    write_step::before(write_step::Step::CreateTemp)?;
    // No repository: stage at the top of the folder, under a name of our own. It exists only
    // until the rename a moment later; one a crash leaves is swept on the next open. The top of
    // the folder is on the same volume as every note in it, so the rename stays atomic.
    let mut tmp = match temp_dir(root) {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            NamedTempFile::new_in(&dir)?
        }
        None => tempfile::Builder::new()
            .prefix(LOOSE_TEMP_PREFIX)
            .tempfile_in(root)?,
    };
    write_step::before(write_step::Step::WriteTemp)?;
    tmp.write_all(content)?;
    // Flush down to physical storage, not just the OS buffer (fsync). Only then is the
    // content guaranteed after the rename even under power loss — persist alone has no durability.
    write_step::before(write_step::Step::Sync)?;
    tmp.as_file().sync_all()?;
    // persist is a rename within the same volume (both under the vault root), so it is atomic and
    // replaces the existing file — or, landing new, refuses to.
    persist(tmp, path, landing)
}

/// Renames `tmp` onto `path`, waiting out a refusal that passes on its own
/// ([`patiently`](crate::fs_ops::patiently)) — failing the save on it would say the edits were
/// not saved when a moment later they would have been.
fn persist(tmp: NamedTempFile, path: &Path, landing: Landing) -> io::Result<()> {
    let mut tmp = Some(tmp);
    crate::fs_ops::patiently(|| {
        write_step::before(write_step::Step::Rename)?;
        let file = tmp.take().expect("put back after every refused attempt");
        let landed = match landing {
            Landing::Replace => file.persist(path),
            Landing::New => file.persist_noclobber(path),
        };
        match landed {
            Ok(_) => Ok(()),
            Err(e) => {
                tmp = Some(e.file);
                Err(e.error)
            }
        }
    })
}

/// The points between the steps of an atomic write. In a shipped build they do nothing; tests
/// use them to see the disk as a crash at that instant would leave it, or to make the next step
/// fail.
pub(crate) mod write_step {
    use std::io;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        CreateTemp,
        WriteTemp,
        Sync,
        Rename,
    }

    #[cfg(all(not(test), not(debug_assertions)))]
    #[inline(always)]
    pub(crate) fn before(_: Step) -> io::Result<()> {
        Ok(())
    }

    /// Development builds only: while the file named by `TEXTREE_E2E_STALL_FLAG` exists, every
    /// write made through these steps waits just before its rename — how the end-to-end tests
    /// make a folder stop answering.
    #[cfg(all(not(test), debug_assertions))]
    pub(crate) fn before(step: Step) -> io::Result<()> {
        if step == Step::Rename {
            if let Some(flag) = std::env::var_os("TEXTREE_E2E_STALL_FLAG") {
                let flag = std::path::PathBuf::from(flag);
                while flag.exists() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    type Hook = Box<dyn FnMut(Step) -> io::Result<()>>;

    #[cfg(test)]
    thread_local! {
        static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    pub(crate) fn before(step: Step) -> io::Result<()> {
        HOOK.with(|h| match h.borrow_mut().as_mut() {
            Some(hook) => hook(step),
            None => Ok(()),
        })
    }

    /// Runs `hook` before every step of atomic writes made on this thread, until the returned
    /// guard is dropped — also when an assertion panics, so no later test inherits it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn install(hook: impl FnMut(Step) -> io::Result<()> + 'static) -> Installed {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        Installed
    }

    #[cfg(test)]
    pub(crate) struct Installed;

    #[cfg(test)]
    impl Drop for Installed {
        fn drop(&mut self) {
            HOOK.with(|h| *h.borrow_mut() = None);
        }
    }
}

/// Seed note written into a freshly created default vault. English only (public repo).
const WELCOME_MD: &str = "---\ntitle: Welcome to Textree\nicon: 🌳\n---\n\n# Welcome to Textree\n\nThis is your vault — a plain folder of Markdown files on your own disk.\nNo account, no lock-in. What you see here is the default look, with no setup.\n\n## A few things to try\n- Edit this note. Everything is just `.md` you fully own.\n- Create notes and folders from the toolbar on the left.\n- **Already keep notes in another folder?** Click 📁 in the sidebar to open\n  any existing standard Markdown vault.\n\nYou can delete this note anytime.\n";

/// True if `dir` directly contains at least one `*.md` file.
fn has_markdown(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x.eq_ignore_ascii_case("md"))
                == Some(true)
        }),
        Err(_) => false,
    }
}

/// Ensures `base/Textree/` exists and, only when it has no Markdown yet, seeds `welcome.md`
/// atomically. Never overwrites existing content (non-destructive). Returns the vault path.
fn ensure_vault_at(base: &Path) -> Result<PathBuf, (String, Option<i32>)> {
    let vault = base.join("Textree");
    std::fs::create_dir_all(&vault).map_err(|e| (e.to_string(), e.raw_os_error()))?;
    if !has_markdown(&vault) {
        atomic_write(&vault, &vault.join("welcome.md"), WELCOME_MD)
            .map_err(|e| (e.to_string(), e.raw_os_error()))?;
    }
    Ok(vault)
}

/// Ordered candidate base directories under which the default `Textree/` vault may live.
/// Dev/E2E: a single forced `TEXTREE_DEFAULT_VAULT_BASE` (test isolation, mirrors TEXTREE_CANOPY_CLI;
/// no fallback so tests stay deterministic). Production: Documents first (preferred), then the home
/// dir, then app-local-data as a last resort. Documents can resolve to an *invalid* path (e.g. a
/// OneDrive-redirected/uninitialized Known Folder) that exists in name but cannot be created under —
/// so we keep going past it rather than trusting the first resolved path.
fn candidate_vault_bases(app: &AppHandle) -> Vec<PathBuf> {
    if let Some(p) = crate::dev_env::var("TEXTREE_DEFAULT_VAULT_BASE") {
        if !p.is_empty() {
            return vec![PathBuf::from(p)];
        }
    }
    let mut bases: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !bases.contains(&dir) {
            bases.push(dir);
        }
    };
    if let Ok(d) = app.path().document_dir() {
        push(d);
    }
    if let Ok(h) = app.path().home_dir() {
        push(h);
    }
    if let Ok(a) = app.path().app_local_data_dir() {
        push(a);
    }
    bases
}

/// Tries each candidate base in order, returning the first under which a vault was successfully
/// created/seeded. `fell_back` is true when that base was not the first (preferred) candidate —
/// i.e. the Documents location was unusable and the vault landed elsewhere, which the UI surfaces
/// so the user always knows where their notes live (data sovereignty). Errors only when *every*
/// candidate fails — never silently no-ops into a blank app.
fn first_creatable_vault(bases: &[PathBuf]) -> Result<(PathBuf, bool), (String, Option<i32>)> {
    let mut last_err = (
        "no candidate base directory could be resolved".to_string(),
        None,
    );
    for (i, base) in bases.iter().enumerate() {
        match ensure_vault_at(base) {
            Ok(vault) => return Ok((vault, i > 0)),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Result of resolving the default vault: its absolute path plus whether the preferred (Documents)
/// location was unusable so the vault was created in a fallback location instead.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultVault {
    pub path: String,
    pub fell_back: bool,
}

/// Ensures the default vault exists (creating it and seeding `welcome.md` on first run) and
/// returns its absolute path. The single backend owner of OS-path resolution + seeding. Falls
/// back through home → app-local-data when Documents is unusable so first run never blank-screens.
pub fn ensure_default_vault(app: AppHandle) -> Result<DefaultVault, String> {
    let bases = candidate_vault_bases(&app);
    match first_creatable_vault(&bases) {
        Ok((vault, fell_back)) => {
            if fell_back {
                log::warn!(
                    "ensure_default_vault: preferred base unusable, fell back to {}",
                    vault.display()
                );
            } else {
                log::info!("ensure_default_vault: {}", vault.display());
            }
            Ok(DefaultVault {
                path: vault.display().to_string(),
                fell_back,
            })
        }
        Err((msg, os_error_code)) => {
            crate::telemetry::record(crate::telemetry::vault_open_failed_report(os_error_code));
            Err(msg)
        }
    }
}

/// Where settings that belong to the person rather than to the notes are kept.
///
/// Outside the notes folder on purpose: a preference is not a note, it is not something anyone
/// wants to see in their folder, and it should not travel when the notes do. Keyed per folder,
/// because "which notes are favourites" only means anything about one set of notes.
fn personal_dir(root: &Path) -> Result<PathBuf, String> {
    let base = crate::dev_env::var_os("TEXTREE_PERSONAL_BASE")
        .map(PathBuf::from)
        .or_else(personal_base)
        .ok_or_else(|| "cannot find a place to keep settings".to_string())?;
    Ok(base.join(".textree").join("vaults").join(folder_key(root)))
}

#[cfg(not(test))]
fn personal_base() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Under test, somewhere of this run's own — writing settings must never reach the home
/// directory of whoever is running the suite.
#[cfg(test)]
fn personal_base() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static ELSEWHERE: OnceLock<tempfile::TempDir> = OnceLock::new();
    Some(
        ELSEWHERE
            .get_or_init(|| tempfile::TempDir::new().expect("a place to keep test settings"))
            .path()
            .to_path_buf(),
    )
}

/// Builds the path of one settings file. Anything other than a plain name component is rejected,
/// so nothing can address a location outside the directory it is confined to.
fn sidecar_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    crate::vault_paths::plain(rel).map_err(|_| "invalid settings path".to_string())?;
    Ok(rel
        .split('/')
        .fold(personal_dir(root)?, |p, name| p.join(name)))
}

/// The same file as it was kept before settings moved out of the notes folder.
fn legacy_sidecar_path(root: &Path, rel: &str) -> PathBuf {
    root.join(".textree").join(rel)
}

const TRASH_MANIFEST: &str = "trash.json";

/// One trashed node's provenance. Lives in `.textree/trash.json` (sidecar, regeneratable
/// in spirit: if lost, the trash files themselves remain the truth).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashItem {
    /// Actual file/dir name inside `.textree/trash/` (collision-disambiguated).
    pub trash_name: String,
    /// Vault-root-relative original path, `/`-separated (portable across OS).
    pub original_rel: String,
    /// Unix epoch seconds at deletion.
    pub deleted_at: u64,
    pub is_dir: bool,
}

/// Reads `.textree/trash.json`. Absent or corrupt → empty (graceful; trash files are the truth).
fn read_trash_manifest(root: &Path) -> Vec<TrashItem> {
    // Stays beside the trash it describes: the two are one thing, and the whole thing is
    // carried over at once rather than half of it at a time.
    let path = legacy_sidecar_path(root, TRASH_MANIFEST);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Reads one of this folder's settings. `None` when it has never been written.
pub fn read_sidecar(root: String, rel: String) -> Result<Option<String>, String> {
    let path = sidecar_path(Path::new(&root), &rel)?;
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Writes one of this folder's settings, leaving the folder itself untouched. Refused while they
/// are in a format a newer release wrote (see [`prepare_sidecar`]).
pub fn write_sidecar(root: String, rel: String, content: String) -> Result<(), String> {
    let root = Path::new(&root);
    let path = sidecar_path(root, &rel)?;
    if !crate::state_dir::writable(&personal_dir(root)?).map_err(|e| e.to_string())? {
        return Err(NEWER_SETTINGS.into());
    }
    atomic_write_beside(&path, &content).map_err(|e| e.to_string())
}

const NEWER_SETTINGS: &str =
    "this folder's settings were saved by a newer Textree and are kept as they are";

/// What opening this folder's settings found.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SidecarState {
    /// They were saved by a newer release: shown as defaults, and nothing is written over them
    /// until the application is updated.
    pub read_only: bool,
}

/// Brings this folder's settings to the format this release keeps them in, when the folder is
/// opened — before anything reads them.
pub fn prepare_sidecar(root: String) -> Result<SidecarState, String> {
    let dir = personal_dir(Path::new(&root))?;
    let current = crate::state_dir::bring_up_to_date(&dir, &root, atomic_bytes_beside)
        .map_err(|e| e.to_string())?;
    Ok(SidecarState {
        read_only: !current,
    })
}

/// Moves one of this folder's settings files that could not be read out of the way, keeping it.
/// Returns the name it now has.
pub fn set_aside_sidecar(root: String, rel: String) -> Result<String, String> {
    let root = Path::new(&root);
    sidecar_path(root, &rel)?; // the same confinement as reading and writing
    let dir = personal_dir(root)?;
    if !crate::state_dir::writable(&dir).map_err(|e| e.to_string())? {
        return Err(NEWER_SETTINGS.into());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let to = crate::state_dir::set_aside(&dir, &rel, now).map_err(|e| e.to_string())?;
    log::warn!(
        "set_aside_sidecar: {rel} could not be read; kept as {}",
        to.display()
    );
    Ok(to
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default())
}

/// An edit kept because it could not be written to its note yet (see `stranded`).
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StrandedEdit {
    pub id: String,
    /// The note it was typed into, relative to the folder.
    pub rel: String,
    pub text: String,
    pub base: String,
}

fn stranded_dir(root: &Path) -> Result<PathBuf, String> {
    Ok(personal_dir(root)?.join(crate::stranded::DIR))
}

/// Keeps an edit that could not be written to its note yet, until it is. Returns its id.
#[tauri::command]
pub async fn keep_stranded(
    root_id: String,
    rel: String,
    text: String,
    base: String,
) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || {
        // From the text alone: the folder may be the very one that stopped answering.
        crate::vault_paths::plain(&rel)?;
        let root = PathBuf::from(root);
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let kept = crate::stranded::Kept {
            rel,
            text,
            base,
            at,
        };
        crate::stranded::keep(&stranded_dir(&root)?, &kept, tauri_kit_fs::write_atomic_new)
            .map_err(|e| e.to_string())
    })
    .await
}

/// Lets go of a kept edit once it has landed.
#[tauri::command]
pub async fn forget_stranded(root_id: String, id: String) -> Result<(), String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || {
        crate::stranded::forget(&stranded_dir(Path::new(&root))?, &id).map_err(|e| e.to_string())
    })
    .await
}

/// Every edit kept for this folder, oldest first, to be tried again now that it is open.
#[tauri::command]
pub async fn list_stranded(root_id: String) -> Result<Vec<StrandedEdit>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || stranded_edits(Path::new(&root))).await
}

fn stranded_edits(root: &Path) -> Result<Vec<StrandedEdit>, String> {
    Ok(crate::stranded::list(&stranded_dir(root)?)
        .into_iter()
        .map(|(id, kept)| StrandedEdit {
            id,
            rel: kept.rel,
            text: kept.text,
            base: kept.base,
        })
        .collect())
}

/// Runs `work` off the main thread. Anything that touches the notes folder goes through here: a
/// folder that stops answering (a stalled sync or network drive) must hold up that one request, not
/// the window.
pub(crate) async fn off_main<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn list_tree(root_id: String) -> Result<Vec<TreeItem>, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || tree_of(root.clone()).and_then(|tree| tree_out(&root, tree))).await
}

/// A node as the screen gets it: its paths relative to the folder.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TreeItem {
    pub name: String,
    pub kind: vault::NodeKind,
    pub path: String,
    pub body_path: Option<String>,
    pub children: Vec<TreeItem>,
}

/// `tree`, a tree built on the folder at `root`, as the screen gets it.
pub(crate) fn tree_out(root: &str, tree: Vec<TreeNode>) -> Result<Vec<TreeItem>, String> {
    tree.into_iter()
        .map(|node| {
            Ok(TreeItem {
                name: node.name,
                kind: node.kind,
                path: crate::vault_paths::rel_out(root, &node.path)?,
                body_path: match node.body_path {
                    Some(p) => Some(crate::vault_paths::rel_out(root, &p)?),
                    None => None,
                },
                children: tree_out(root, node.children)?,
            })
        })
        .collect()
}

/// A change to a note, as the screen is told about it: which folder, and where in it.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct FsEvent {
    #[serde(rename = "rootId")]
    pub root_id: String,
    pub kind: crate::watcher::ChangeKind,
    pub rel: String,
}

/// The folder's whole tree has to be read again: said with the folder it is about.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct RescanEvent {
    #[serde(rename = "rootId")]
    pub root_id: String,
}

pub(crate) fn tree_of(root: String) -> Result<Vec<TreeNode>, String> {
    vault::build_tree(Path::new(&root)).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn read_note(root_id: String, rel: String) -> Result<String, String> {
    let root = crate::workspace::path_of(&root_id)?;
    off_main(move || {
        let path = crate::vault_paths::resolve(&root, &rel)?;
        note_text(root, path.to_string_lossy().into_owned())
    })
    .await
}

pub(crate) fn note_text(root: String, path: String) -> Result<String, String> {
    // Resolved inside the vault by the command (`vault_paths`).
    let _ = root;
    std::fs::read_to_string(PathBuf::from(path)).map_err(|e| e.to_string())
}

/// What became of a save.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WriteOutcome {
    /// The note now holds the new content.
    Written,
    /// The note no longer held what the save was based on, so nothing was written. `disk` is what
    /// it holds instead, or `None` when it is gone.
    Conflict { disk: Option<String> },
}

/// Why a save would not replace the note, if it would not: the note must still hold `expected`,
/// the content the editor and disk last agreed on. Anything else means someone changed it since,
/// and writing would silently throw that change away.
///
/// The check and the rename are not one atomic step — nothing on an ordinary filesystem offers
/// that — but they are microseconds apart in one process, instead of the round trip between the
/// editor reading a note and its next save arriving.
fn stale_base(path: &Path, expected: &str) -> io::Result<Option<WriteOutcome>> {
    match std::fs::read_to_string(path) {
        Ok(disk) if disk == expected => Ok(None),
        Ok(disk) => Ok(Some(WriteOutcome::Conflict { disk: Some(disk) })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Ok(Some(WriteOutcome::Conflict { disk: None }))
        }
        Err(e) => Err(e),
    }
}

/// Saves a note, off the main thread: a folder that stops answering (a stalled sync or network
/// drive) holds up this save, not the window or the other notes.
#[tauri::command]
pub async fn write_note(
    root_id: String,
    rel: String,
    content: String,
    expected: String,
    own: State<'_, tauri_kit_watch::OwnWrites>,
    locks: State<'_, Arc<NoteLocks>>,
    app: AppHandle,
) -> Result<WriteOutcome, String> {
    let root = crate::workspace::path_of(&root_id)?;
    let (own, locks) = (own.inner().clone(), locks.inner().clone());
    off_main(move || {
        let path = crate::vault_paths::resolve(&root, &rel)?;
        let root = PathBuf::from(root);
        let outcome = save_note(&root, &path, &content, &expected, &own, &locks)?;
        if outcome == WriteOutcome::Written {
            refresh_indexes(&app, root, path);
        }
        Ok(outcome)
    })
    .await
}

/// Replaces the note with `content` if it still holds `expected`. Saves to one note run one at a
/// time, each checking against what the previous one left (see [`NoteLocks`]).
pub(crate) fn save_note(
    root: &Path,
    path: &Path,
    content: &str,
    expected: &str,
    own: &tauri_kit_watch::OwnWrites,
    locks: &NoteLocks,
) -> Result<WriteOutcome, String> {
    // The whole folder is gone — the note with it. Nothing can be written, nothing is.
    if !root.is_dir() {
        log::info!(
            "write_note: the folder {} is gone; not written",
            root.display()
        );
        return Ok(WriteOutcome::Conflict { disk: None });
    }
    // `path` was resolved inside the vault by the command (`vault_paths` — a note deleted outside
    // the app resolves too, and comes back as "gone" below).
    let _turn = locks.turn(path);
    if let Some(conflict) = stale_base(path, expected).map_err(|e| e.to_string())? {
        log::info!(
            "write_note: {} changed since it was loaded; not written",
            path.display()
        );
        return Ok(conflict);
    }
    // Recorded before writing: the watch can hear about the write before it returns, and would
    // otherwise report the app's own save back to it as someone else's change.
    own.record(path, content.as_bytes());
    match atomic_write(root, path, content) {
        Ok(()) => {
            log::info!("write_note: {} ({} bytes)", path.display(), content.len());
            Ok(WriteOutcome::Written)
        }
        Err(e) => {
            // The disk did not change: what was recorded is not what is there.
            own.forget(path);
            log::error!("write_note failed for {}: {}", path.display(), e);
            Err(e.to_string())
        }
    }
}

/// Brings the search indexes up to date with a saved note, in the background: the full-text index
/// can be held for a whole rebuild, and the save is already on disk — it must not wait for that.
/// The watcher skips the app's own writes, so in-app edits reach the index only through here.
///
/// The note is read again rather than passed along: two saves in quick succession start two of
/// these, in no guaranteed order, and whichever runs last must index what the note holds now.
/// An index failure does not fail anything (the index is a derived cache).
fn refresh_indexes(app: &AppHandle, root: PathBuf, path: PathBuf) {
    let index = app.state::<Arc<IndexHandle>>().inner().clone();
    let host = app.state::<Arc<host::HostHandle>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Ok(content) = std::fs::read_to_string(&path) else {
            return;
        };
        if let Some(state) = index.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = state.index_note(&root, &path, &content);
        }
        // Background semantic index. Runs without asking because it only reads: it derives a
        // search index and never alters what the person wrote.
        host::index_note(&host, &root.to_string_lossy(), &path.to_string_lossy());
    });
}

/// Which opening of a folder is the latest. Opening runs off the main thread, so a folder that
/// does not answer no longer holds the window — and the person can open another one meanwhile.
/// Then the later one is what they want: the earlier one, when it finally gets through, installs
/// nothing.
static OPENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Taken while an opening puts its folder in place, so two cannot interleave that.
static INSTALLING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What an opening that another one overtook reports. The window, having started the later one,
/// does not show it.
pub(crate) const SUPERSEDED: &str = "another folder was opened meanwhile";

/// Development builds only: while the file named by `TEXTREE_E2E_OPEN_STALL_FLAG` exists and holds
/// this folder's path, opening it waits before reading anything — how the end-to-end tests make a
/// folder stop answering when it is opened.
#[cfg(all(not(test), debug_assertions))]
fn stall_opening(root: &str) {
    let Some(flag) = std::env::var_os("TEXTREE_E2E_OPEN_STALL_FLAG") else {
        return;
    };
    while std::fs::read_to_string(&flag).is_ok_and(|named| named.trim() == root) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(any(test, not(debug_assertions)))]
#[inline(always)]
fn stall_opening(_: &str) {}

/// Starts an opening; the number it returns is what [`open_vault`] checks before installing.
pub(crate) fn begin_open() -> u64 {
    OPENS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

/// Opens a folder: reads its tree, installs its search index, points the semantic index at it and
/// watches it. Everything that reads the folder runs first, where it may take as long as the
/// folder takes; only then, briefly and one opening at a time, is the folder put in place — and
/// only if no later opening started meanwhile.
pub fn open_vault(
    root: String,
    root_id: String,
    app: AppHandle,
    ticket: u64,
) -> Result<Vec<TreeNode>, String> {
    stall_opening(&root);
    let root_path = PathBuf::from(&root);
    // Sweep orphaned atomic-write temps (crash/power-loss leftovers) so they don't linger and sync.
    clear_temp_dir(&root_path);
    let tree = vault::build_tree(&root_path).map_err(|e| e.to_string())?;
    log::info!("open_vault: {} ({} top-level nodes)", root, tree.len());

    let own = app.state::<tauri_kit_watch::OwnWrites>().inner().clone();
    let index = app.state::<Arc<IndexHandle>>().inner().clone();
    let host = app.state::<Arc<host::HostHandle>>().inner().clone();
    let app_data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let emitter = app.clone();
    let watched = root.clone();
    let watched_id = root_id.clone();
    use tauri::Emitter as _;
    let watcher =
        crate::watcher::watch_vault(
            &root_path,
            &own,
            index.clone(),
            move |surface| match surface {
                crate::watcher::Surface::Changed(batch) => {
                    for change in batch {
                        // Said with the folder it happened in: one that arrives after another folder was
                        // opened is not mistaken for a note of the same name there.
                        let Some(rel) =
                            crate::vault_paths::rel_of(&watched, Path::new(&change.path))
                        else {
                            continue;
                        };
                        let _ = emitter.emit(
                            "fs_changed",
                            FsEvent {
                                root_id: watched_id.clone(),
                                kind: change.kind,
                                rel,
                            },
                        );
                    }
                }
                crate::watcher::Surface::Rescan => {
                    let _ = emitter.emit(
                        "fs_rescan",
                        RescanEvent {
                            root_id: watched_id.clone(),
                        },
                    );
                }
            },
        )?;

    let _installing = INSTALLING.lock().unwrap_or_else(|e| e.into_inner());
    if OPENS.load(std::sync::atomic::Ordering::SeqCst) != ticket {
        log::info!("open_vault: {root} was overtaken by a later opening; not installed");
        return Err(SUPERSEDED.into()); // the watch it started stops as it drops
    }

    // Install the index (app data directory, per-vault hash). On failure only search is disabled —
    // graceful degradation (editing, tree, and file search remain intact without the index).
    let dir = crate::search::index_dir(&app_data, &root_path);
    match IndexState::open_or_create(&dir, &root_path) {
        Ok(state) => {
            let was_empty = state.is_empty().unwrap_or(true);
            *index.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
            if was_empty {
                // Full build in the background (non-blocking for the UI).
                let index_arc = index.clone();
                let root_for_build = root_path.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    if let Some(st) = index_arc
                        .0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                    {
                        let _ = st.rebuild(&root_for_build);
                    }
                });
            }
        }
        Err(e) => {
            log::warn!("failed to open index (search disabled): {e}");
            *index.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    // Background semantic reindex. Runs without asking for the same reason as the per-note
    // index above: it only reads, and changes nothing a person wrote.
    host.set_current_vault(root.clone());
    let vault_str = root.clone();
    tauri::async_runtime::spawn_blocking(move || {
        host::reindex_vault(&host, &vault_str);
    });

    // The previous watch stops as it is replaced, so leftover changes from the old vault don't
    // bleed into the new.
    let watcher_handle = app.state::<WatcherHandle>();
    let previous = watcher_handle
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(watcher);
    drop(previous);

    Ok(tree)
}

// ── Structural edits (M4) — delegated to fs_ops ────────────────────────────────

/// Drops the include choices left at or under vault-relative `rels` — by a note that went away,
/// for one that is just made there: a path that comes into existence never inherits an include it
/// did not ask for (only a move carries a choice along). A leave-out is kept: it can only hold
/// back, and a left-out note brought back from Deleted notes stays left out.
///
/// Best-effort, like [`record_moves`]: the change on disk already happened. No repository is
/// created — a folder whose notes were never recorded has no choices to drop.
fn forget_choices(root: &Path, rels: &[String]) {
    let prepared = match crate::git_engine::open_existing(root) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            log::warn!(
                "forget_choices: cannot open the repository: {}",
                e.message()
            );
            return;
        }
    };
    let paths: Vec<PathBuf> = rels
        .iter()
        .map(|r| prepared.path_in_repo(Path::new(r)))
        .collect();
    if let Err(e) = crate::publish_scope::vacate(prepared.repo(), &paths) {
        log::warn!(
            "forget_choices: publish choices not cleared: {}",
            e.message()
        );
    }
}

/// [`forget_choices`] for something just made at `created`.
fn forget_choices_at(root: &Path, created: &Path) {
    match crate::vault_paths::rel_out(root, created) {
        Ok(rel) => forget_choices(root, &[rel]),
        Err(e) => log::warn!("forget_choices: {}: {e}", created.display()),
    }
}

pub fn create_note(root: String, parent: String, name: String) -> Result<String, String> {
    let path = crate::fs_ops::create_note(Path::new(&parent), &name).map_err(|e| e.to_string())?;
    log::info!("create_note: {}", path.display());
    forget_choices_at(Path::new(&root), &path);
    Ok(path.display().to_string())
}

pub fn create_untitled_note(root: String, parent: String) -> Result<String, String> {
    let path =
        crate::fs_ops::create_untitled_note(Path::new(&parent)).map_err(|e| e.to_string())?;
    log::info!("create_untitled_note: {}", path.display());
    forget_choices_at(Path::new(&root), &path);
    Ok(path.display().to_string())
}

pub fn create_note_with_content(
    root: String,
    parent: String,
    name: String,
    content: String,
) -> Result<String, String> {
    let path = crate::fs_ops::create_note_with_content(Path::new(&parent), &name, &content)
        .map_err(|e| e.to_string())?;
    log::info!("create_note_with_content: {}", path.display());
    forget_choices_at(Path::new(&root), &path);
    Ok(path.display().to_string())
}

pub fn rename_note_unique(root: String, path: String, name: String) -> Result<String, String> {
    let root_p = Path::new(&root);
    let from_rel = crate::vault_paths::rel_out(root_p, Path::new(&path)).ok();
    let p =
        crate::fs_ops::rename_note_unique(Path::new(&path), &name).map_err(|e| e.to_string())?;
    log::info!("rename_note_unique: {} -> {}", path, p.display());
    // A rename like any other: what was recorded and chosen for the note goes with it.
    if let (Some(from), Ok(to)) = (from_rel, crate::vault_paths::rel_out(root_p, &p)) {
        record_moves(root_p, &[(from, to)]);
    }
    Ok(p.display().to_string())
}

pub fn create_folder(root: String, parent: String, name: String) -> Result<String, String> {
    let dir = crate::fs_ops::create_folder(Path::new(&parent), &name).map_err(|e| e.to_string())?;
    log::info!("create_folder: {}", dir.display());
    forget_choices_at(Path::new(&root), &dir);
    Ok(dir.display().to_string())
}

pub fn promote_node(root: String, path: String) -> Result<String, String> {
    let root_p = Path::new(&root);
    // Resolved first: promoting replaces the file this names.
    let from_rel = crate::vault_paths::rel_out(root_p, Path::new(&path)).ok();
    let dir = crate::fs_ops::promote_leaf(Path::new(&path)).map_err(|e| e.to_string())?;
    // The folder is new: it inherits no include left at its name (the note's own choice is
    // carried below).
    forget_choices_at(root_p, &dir);
    // The note did not disappear, it went one level down and took its history with it.
    if let (Some(from), Some(to)) = (from_rel, folder_note_rel(root_p, &dir)) {
        record_moves(root_p, &[(from, to)]);
    }
    Ok(dir.display().to_string())
}

/// The vault-relative path of the note a folder is named after.
fn folder_note_rel(root: &Path, dir: &Path) -> Option<String> {
    let stem = dir.file_name().and_then(|s| s.to_str())?;
    let dir_rel = crate::vault_paths::rel_out(root, dir).ok()?;
    Some(format!("{dir_rel}/{stem}.md"))
}

/// Vault-root-relative, `/`-separated path of an existing target (for manifest portability).
/// Every file at or under `target`, as (path on disk, path within the repository) pairs.
fn files_under(
    prepared: &crate::git_engine::VaultRepo,
    root: &Path,
    target: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut found = Vec::new();
    let mut stack = vec![target.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&current) {
                for entry in entries.flatten() {
                    stack.push(entry.path());
                }
            }
            continue;
        }
        let Ok(rel) = crate::vault_paths::rel_out(root, &current) else {
            continue;
        };
        found.push((current, prepared.path_in_repo(Path::new(&rel))));
    }
    found
}

/// Keeps the state of everything at or under a path, and records that it happened.
///
/// Everything is kept, not only what history lacks. Two reasons, and each alone is enough:
///
/// - Being recorded once says nothing about the state on disk now. A note recorded, then edited
///   without being recorded again, then deleted, has its newer state nowhere else — skipping it
///   because the path appears in history loses exactly the work that was never saved.
/// - The revision is also the only record of *when* something left the folder. A deletion that
///   writes nothing leaves no answer to that question, and there is no other place holding it.
///
/// Called before anything that replaces what is on disk, not only before deleting: going back to
/// an earlier state overwrites the current one just as finally.
///
/// Contents that history already holds cost nothing to keep again: identical contents are one
/// object either way. Nothing written here is part of the history anyone reads.
pub(crate) fn keep_state_of(root: &Path, target: &Path) -> Result<(), String> {
    // A repository is made where none governs the folder, so failing here means one is there and
    // cannot be opened (or none can be made). Then there is nowhere to keep anything, and going
    // ahead would destroy the only copy: refuse, the same as for a file that cannot be read.
    let prepared = crate::git_engine::prepare(root).map_err(|e| {
        log::warn!("keep_state_of: no repository available: {}", e.message());
        format!(
            "Nothing was deleted: this folder's version history could not be opened, so there is nowhere to keep a copy to bring it back from ({})",
            e.message()
        )
    })?;

    let mut entries: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for (on_disk, in_repo) in files_under(&prepared, root, target) {
        match std::fs::read(&on_disk) {
            Ok(content) => entries.push((in_repo, content)),
            Err(e) => {
                // A file that cannot be read cannot be kept. Say so rather than deleting it
                // while reporting success.
                return Err(format!("'{}' could not be read: {e}", on_disk.display()));
            }
        }
    }
    if entries.is_empty() {
        return Ok(());
    }

    let (author, committer) = crate::git_engine::commit_identities(prepared.repo())
        .map_err(|e| e.message().to_string())?;
    crate::git_engine::commit_paths(
        prepared.repo(),
        crate::git_engine::SNAPSHOT_REF,
        &entries,
        "keep deleted content",
        &author,
        &committer,
        // Deleting the same contents twice is two events, not one. Skipping the second would
        // leave the list showing the first deletion's moment for something deleted later.
        crate::git_engine::WhenUnchanged::Record,
    )
    .map_err(|e| e.message().to_string())?;
    log::info!("keep_state_of: kept {} file(s)", entries.len());
    Ok(())
}

/// Deletes a note or folder, once its state is somewhere it can be brought back from.
///
/// Nothing is set aside inside the notes folder. A copy there was how a deletion used to be
/// undoable; the history is that now. Keeping both would leave two places offering to bring the
/// same note back — and a folder holding something that is not a note.
///
/// The order is not an implementation detail: keeping comes first, because afterwards there is
/// nowhere left to read the state from.
pub fn delete_node(root: String, path: String) -> Result<(), String> {
    let root_p = Path::new(&root);
    let target = Path::new(&path);
    if std::fs::canonicalize(target).ok() == std::fs::canonicalize(root_p).ok() {
        return Err("the folder itself cannot be deleted".into());
    }
    crate::vault_paths::rel_out(root_p, target)?;
    // Nothing whose name begins with a dot, at any depth. The tree never offers these, but this
    // is where paths are checked, and the cost of the check being somewhere else is total: asked
    // to delete the repository, this would read all of it into a commit, write that commit inside
    // the very directory it is about to remove, and then remove it — losing the notes, their
    // history, and the copy just made of them in one step.
    if target
        .strip_prefix(root_p)
        .unwrap_or(target)
        .components()
        .any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with('.')))
    {
        return Err("that is not a note".into());
    }
    // Resolved while it is still there to resolve.
    let rel = crate::vault_paths::rel_out(root_p, target).ok();
    keep_state_of(root_p, target)?;
    if target.is_dir() {
        crate::fs_ops::patiently(|| std::fs::remove_dir_all(target)).map_err(|e| e.to_string())?;
    } else {
        crate::fs_ops::patiently(|| std::fs::remove_file(target)).map_err(|e| e.to_string())?;
    }
    log::info!("delete_node: {}", path);
    // An include goes with it: something made at the same place later did not ask to be
    // published. A leave-out stays, so the note brought back is still left out.
    if let Some(rel) = rel {
        forget_choices(root_p, &[rel]);
    }
    Ok(())
}

/// Re-validates a vault-relative path from the (user-editable) manifest: every component must be
/// Normal and a valid name. "Security at the boundary" — the manifest is not trusted input.
fn validate_vault_rel(rel: &str) -> Result<(), String> {
    let p = Path::new(rel);
    let mut any = false;
    for comp in p.components() {
        match comp {
            Component::Normal(s) => {
                any = true;
                if !crate::pathsafe::is_valid_name(&s.to_string_lossy()) {
                    return Err("invalid path segment in trash entry".into());
                }
            }
            _ => return Err("invalid trash path (non-normal component)".into()),
        }
    }
    if !any {
        return Err("empty trash path".into());
    }
    Ok(())
}

/// Carries whatever was recorded along a set of moves, in every reference that holds it.
///
/// The pairs are vault-relative and applied in order, so an operation that is several moves on
/// disk stays one revision here.
///
/// Best-effort by design: the files have already moved by the time this runs, and a history
/// that could not keep up is not a reason to report the move as having failed. It is reported
/// in the log instead.
///
/// No repository is created here. A folder nothing was ever recorded in has no history for a
/// move to affect, and moving a file around is not the moment to start one.
fn record_moves(root: &Path, pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    let prepared = match crate::git_engine::open_existing(root) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            log::warn!("record_moves: cannot open the repository: {}", e.message());
            return;
        }
    };
    let repo = prepared.repo();
    let moves: Vec<(PathBuf, PathBuf)> = pairs
        .iter()
        .map(|(from, to)| {
            (
                prepared.path_in_repo(Path::new(from)),
                prepared.path_in_repo(Path::new(to)),
            )
        })
        .collect();
    let Ok((author, committer)) = crate::git_engine::commit_identities(repo) else {
        log::warn!("record_moves: no identity available");
        return;
    };
    // Both references are carried over. What was kept when a note was deleted describes the
    // same note, so leaving it at the old name would make a note that came back and was then
    // renamed look deleted all over again.
    for reference in [
        crate::git_engine::NOTES_REF,
        crate::git_engine::SNAPSHOT_REF,
    ] {
        match crate::git_engine::move_recorded(repo, reference, &moves, &author, &committer) {
            Ok(Some(_)) => log::info!("record_moves: {} pair(s) in {reference}", pairs.len()),
            Ok(None) => {}
            Err(e) => log::warn!("record_moves: {reference} not updated: {}", e.message()),
        }
    }
    // A note or folder that was chosen for publishing stays chosen where it went.
    if let Err(e) = crate::publish_scope::follow_moves(repo, &moves) {
        log::warn!("record_moves: publish choices not carried: {}", e.message());
    }
}

pub fn rename_node(root: String, path: String, name: String) -> Result<String, String> {
    let root_p = Path::new(&root);
    let target = Path::new(&path);
    // Read before the move: resolving a path needs it to still be where it is. A folder also
    // carries a note named after it, and that note is renamed too — two moves, one revision.
    let from_rel = crate::vault_paths::rel_out(root_p, target).ok();
    // Only a note the folder really has: a pair for one it lacks would clear whatever choice the
    // destination name holds (a different note of that name inside the folder).
    let folder_note = target
        .is_dir()
        .then(|| {
            target
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .flatten()
        .filter(|old| target.join(format!("{old}.md")).is_file());

    let p = crate::fs_ops::rename_node(target, &name).map_err(|e| e.to_string())?;
    log::info!("rename_node: {} -> {}", path, name);

    if let (Some(from), Ok(to)) = (from_rel, crate::vault_paths::rel_out(root_p, &p)) {
        let mut pairs = vec![(from, to.clone())];
        if let Some(old_name) = folder_note {
            pairs.push((format!("{to}/{old_name}.md"), format!("{to}/{name}.md")));
        }
        record_moves(root_p, &pairs);
    }
    Ok(p.display().to_string())
}

pub fn move_node(root: String, path: String, dest: String) -> Result<String, String> {
    let root_p = Path::new(&root);
    let from_rel = crate::vault_paths::rel_out(root_p, Path::new(&path)).ok();
    let p =
        crate::fs_ops::move_node(Path::new(&path), Path::new(&dest)).map_err(|e| e.to_string())?;
    log::info!("move_node: {} -> {}", path, dest);
    if let (Some(from), Ok(to)) = (from_rel, crate::vault_paths::rel_out(root_p, &p)) {
        record_moves(root_p, &[(from, to)]);
    }
    Ok(p.display().to_string())
}

pub fn adopt_node(root: String, path: String, leaf: String) -> Result<String, String> {
    let root_p = Path::new(&root);
    let leaf_p = Path::new(&leaf);
    let from_rel = crate::vault_paths::rel_out(root_p, Path::new(&path)).ok();
    // The leaf is resolved before it is promoted, because promoting replaces the file it names.
    let leaf_before = crate::vault_paths::rel_out(root_p, leaf_p).ok();

    let p = crate::fs_ops::adopt_into_leaf(Path::new(&path), leaf_p).map_err(|e| e.to_string())?;
    // The folder the promotion made is new: it inherits no include left at its name. Cleared
    // before the moves are carried, so the choices they bring in are kept.
    if let Some(dir) = p.parent() {
        forget_choices_at(root_p, dir);
    }

    // Adopting is a promotion followed by a move, and both have to be carried over.
    let mut pairs = Vec::new();
    if let (Some(before), Some(after)) = (
        leaf_before,
        p.parent().and_then(|d| folder_note_rel(root_p, d)),
    ) {
        pairs.push((before, after));
    }
    if let (Some(from), Ok(to)) = (from_rel, crate::vault_paths::rel_out(root_p, &p)) {
        pairs.push((from, to));
    }
    record_moves(root_p, &pairs);
    Ok(p.display().to_string())
}

/// Saves an attached image. `data` is base64-encoded bytes. Returns the relative link to insert into the body.
pub fn save_attachment(
    root: String,
    note: String,
    data: String,
    ext: String,
) -> Result<String, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    let root_p = Path::new(&root);
    let folder = Path::new(&note).parent().map(|p| p.join("assets"));
    let fresh_folder = folder.as_ref().is_some_and(|f| !f.exists());
    let link = crate::fs_ops::save_attachment(Path::new(&note), &bytes, &ext)
        .map_err(|e| e.to_string())?;
    // What was made: the file, or the folder too when it was made for it.
    if let Some(folder) = folder {
        let made = if fresh_folder {
            folder
        } else {
            folder.join(link.rsplit('/').next().unwrap_or(&link))
        };
        forget_choices_at(root_p, &made);
    }
    Ok(link)
}

/// Off the main thread: the index is held for the whole of a first-open rebuild, and a search made
/// meanwhile waits for it.
#[tauri::command]
pub async fn search_content(
    query: String,
    limit: usize,
    index: State<'_, Arc<IndexHandle>>,
) -> Result<Vec<SearchHit>, String> {
    let index = index.inner().clone();
    off_main(move || {
        let guard = index.0.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(state) => state.search(&query, limit).map_err(|e| e.to_string()),
            None => Ok(Vec::new()), // no index → empty results (graceful)
        }
    })
    .await
}

pub fn rebuild_index(root: String, index: &IndexHandle) -> Result<(), String> {
    let root_path = PathBuf::from(root);
    let mut guard = index.0.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(state) if !state.serves(&root_path) => Err("the index is for another folder".into()),
        Some(state) => state.rebuild(&root_path).map_err(|e| e.to_string()),
        None => Err("index is not installed".into()),
    }
}

/// Resolves how to invoke canopy. Dev/E2E: the `TEXTREE_CANOPY_CLI` env var (path to the CLI script
/// or exe) — a `.js` path is run via `node`. Production: the bundled canopy sidecar (`node` + the
/// installed canopy release) under `<resource>/canopy/`.
fn resolve_canopy(app: &AppHandle) -> Result<crate::publish::CanopyInvocation, String> {
    use crate::publish::CanopyInvocation;
    if let Some(p) = crate::dev_env::var("TEXTREE_CANOPY_CLI") {
        let path = PathBuf::from(&p);
        if path.extension().and_then(|e| e.to_str()) == Some("js") {
            return Ok(CanopyInvocation {
                program: "node".into(),
                prefix_args: vec![path.into_os_string()],
            });
        }
        return Ok(CanopyInvocation {
            program: path.into_os_string(),
            prefix_args: vec![],
        });
    }
    let resource = app.path().resource_dir().map_err(|e| e.to_string())?;
    if let Some(inv) = crate::publish::canopy_from_resource_dir(&resource) {
        return Ok(inv);
    }
    Err("the canopy renderer is not available (set TEXTREE_CANOPY_CLI in dev, or bundle the sidecar)"
        .into())
}

/// The chosen notes, as last added, written to a temporary folder for the renderer.
struct Staged {
    _dir: tempfile::TempDir,
    source: PathBuf,
    assembly: crate::publish_assembly::Assembly,
    prepared: crate::git_engine::VaultRepo,
}

fn stage(vault: &Path) -> Result<Staged, String> {
    let prepared =
        history_repo(vault)?.ok_or("nothing in this folder has been added as a version yet")?;
    let assembly = crate::publish_assembly::assemble(&prepared, vault)?;
    if assembly.nothing_chosen {
        return Err("nothing in this folder is chosen for publishing yet".into());
    }
    if assembly.notes.is_empty() {
        return Err(
            "none of the notes chosen for publishing has been added as a version yet".into(),
        );
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let source = dir.path().join("site-source");
    crate::publish_assembly::write_out(&assembly, &source).map_err(|e| e.to_string())?;
    Ok(Staged {
        _dir: dir,
        source,
        assembly,
        prepared,
    })
}

/// The renderer would name the site after the staged folder; name it after the notes folder.
fn with_default_title(
    mut options: crate::publish::PublishOptions,
    vault: &Path,
) -> crate::publish::PublishOptions {
    if options.site_title.is_none() {
        options.site_title = vault.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    options
}

/// Sends `staged` out with `render`, then records it as the published state — only once it went
/// out. Recording comes after, so a failure there is only logged.
fn publish_staged<T>(
    staged: &Staged,
    render: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let result = render(&staged.source)?;
    if let Err(e) = crate::publish_assembly::record_published(&staged.prepared, &staged.assembly) {
        log::warn!("could not record the published state: {e}");
    }
    Ok(result)
}

/// Publishes the open vault to a static site by spawning canopy. Read-only over the source:
/// the vault `.md` is never mutated; only `out_dir` (which must lie outside the vault) is written.
pub fn publish_site(
    app: AppHandle,
    vault_path: String,
    out_dir: String,
    options: crate::publish::PublishOptions,
) -> Result<crate::publish::PublishResult, String> {
    let vault = PathBuf::from(&vault_path);
    let out = PathBuf::from(&out_dir);
    let canopy = resolve_canopy(&app)?;
    let staged = stage(&vault)?;
    let options = with_default_title(options, &vault);
    let result = publish_staged(&staged, |source| {
        crate::publish::run_publish(
            &vault,
            source,
            &out,
            &options,
            &canopy,
            crate::publish::RENDER_TIMEOUT,
        )
    })?;
    log::info!(
        "publish_site: {} ({} pages)",
        result.out_dir,
        result.page_count
    );
    Ok(result)
}

/// Publishes the open vault to the cloud (pub.textree.me): renders locally via canopy, zips the
/// output, and uploads it to api.textree.me/publish using the stored publish token. Read-only over
/// the source. Errors if no publish token is set (add one in Settings).
pub fn publish_to_cloud(
    app: AppHandle,
    vault_path: String,
    options: crate::publish::PublishOptions,
) -> Result<crate::cloud_publish::PublishToCloudResult, String> {
    let vault = PathBuf::from(&vault_path);
    let canopy = resolve_canopy(&app)?;
    let token = crate::publish_secret::get_token()
        .ok_or("no publish token is set — add one in Settings")?;
    let staged = stage(&vault)?;
    let options = with_default_title(options, &vault);
    let result = publish_staged(&staged, |source| {
        crate::cloud_publish::publish_to_cloud(&vault, source, &options, &canopy, &token)
    })?;
    log::info!(
        "publish_to_cloud: {} ({} pages)",
        result.url,
        result.page_count
    );
    Ok(result)
}

/// What a publish of the folder would send out, shown before anything is sent.
#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PublishPreview {
    /// Nothing in the folder is chosen for publishing.
    pub nothing_chosen: bool,
    /// Chosen notes that go out, as their last added version.
    pub notes: Vec<String>,
    /// Files those notes link to that go out with them.
    pub files: Vec<String>,
    /// Of the notes going out, those whose file holds edits not added as a version.
    pub unrecorded: Vec<String>,
    /// Chosen notes that have no version yet and so stay out.
    pub never_recorded: Vec<String>,
    /// Files linked from published notes that are not in the last added versions.
    pub missing: Vec<String>,
    /// Notes linked from published notes that do not go out.
    pub unpublished_links: Vec<String>,
    /// Linked files left out because their own nearest choice is "leave out".
    pub withheld: Vec<String>,
    /// Linked files left out because they are too large to publish.
    pub too_large: Vec<String>,
}

/// The publish choices of the folder at `root`, relative to it. Nothing is created.
pub fn publish_choices(root: String) -> Result<Vec<crate::publish_scope::Choice>, String> {
    let root_p = Path::new(&root);
    let Some(prepared) = history_repo(root_p)? else {
        return Ok(Vec::new());
    };
    let all = crate::publish_scope::read(prepared.repo(), crate::publish_scope::PUBLIC_REF)
        .map_err(|e| e.message().to_string())?;
    let vault = crate::git_engine::slashed(&prepared.path_in_repo(Path::new("")));
    Ok(crate::publish_scope::for_vault(
        &all,
        vault.trim_end_matches('/'),
    ))
}

/// Records that `rel` (relative to `root`, `""` = the whole folder) is included, left out, or
/// (`None`) follows the folder above it. Returns the folder's choices afterwards.
pub fn set_publish_choice(
    root: String,
    rel: String,
    choice: Option<bool>,
) -> Result<Vec<crate::publish_scope::Choice>, String> {
    let root_p = Path::new(&root);
    // Plain names inside the folder; the note may be gone by now, so it need not exist.
    if !rel.is_empty() {
        crate::vault_paths::plain(&rel)?;
    }
    let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
    let in_repo = crate::git_engine::slashed(&prepared.path_in_repo(Path::new(&rel)));
    crate::publish_scope::set(prepared.repo(), in_repo.trim_end_matches('/'), choice)
        .map_err(|e| e.message().to_string())?;
    publish_choices(root)
}

/// Lists what publishing `vault_path` would send out. Reads only: nothing is recorded, and a
/// folder with no history is not turned into a repository.
pub fn publish_preview(vault_path: String) -> Result<PublishPreview, String> {
    let root = PathBuf::from(&vault_path);
    if !root.is_dir() {
        return Err("the vault path is not a directory".into());
    }
    let Some(prepared) = history_repo(&root)? else {
        return Ok(PublishPreview {
            nothing_chosen: true,
            ..PublishPreview::default()
        });
    };
    let a = crate::publish_assembly::assemble(&prepared, &root)?;
    let mut unrecorded = Vec::new();
    for (rel, recorded) in a.files.iter().filter(|(rel, _)| a.notes.contains(rel)) {
        let mut path = root.clone();
        path.extend(rel.split('/'));
        if std::fs::read(&path).ok().as_deref() != Some(recorded.as_slice()) {
            unrecorded.push(rel.clone());
        }
    }
    Ok(PublishPreview {
        nothing_chosen: a.nothing_chosen,
        notes: a.notes,
        files: a.assets,
        unrecorded,
        never_recorded: a.never_recorded,
        missing: a.missing,
        unpublished_links: a.unpublished_links,
        withheld: a.withheld,
        too_large: a.too_large,
    })
}

// ── Remote (exchange recorded notes with a remote the person connected) ─────────────────────

/// The remote this folder exchanges notes with, as this machine remembers it.
pub fn remote_connection(root: String) -> Result<Option<crate::remote::Connection>, String> {
    let path = personal_dir(Path::new(&root))?.join(crate::remote::CONNECTION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("the remote settings could not be read: {e}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Connects this folder to a remote. The remote is reached with the secret first, and remembered
/// only when that worked — a connection that was saved but cannot be used would fail later, far
/// from where it was set up.
pub fn connect_remote(
    root: String,
    url: String,
    username: String,
    secret: String,
) -> Result<(), String> {
    let root_p = Path::new(&root);
    let url = url.trim().to_string();
    crate::git_transport::check_url(&url).map_err(|e| e.message().to_string())?;
    let username = match username.trim() {
        "" => "textree".to_string(),
        name => name.to_string(),
    };
    let dir = personal_dir(root_p)?;
    if !crate::state_dir::writable(&dir).map_err(|e| e.to_string())? {
        return Err(NEWER_SETTINGS.into());
    }
    let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
    let credentials = crate::git_transport::Credentials {
        username: username.clone(),
        secret: secret.clone(),
    };
    crate::remote::fetch(prepared.repo(), &url, Some(credentials))
        .map_err(|e| e.message().to_string())?;

    let connection = crate::remote::Connection { url, username };
    let text = serde_json::to_string_pretty(&connection).map_err(|e| e.to_string())?;
    crate::remote::set_secret(&folder_key(root_p), &secret)?;
    atomic_write_beside(&dir.join(crate::remote::CONNECTION_FILE), &text).map_err(|e| e.to_string())
}

/// Whether this folder's recorded notes are somewhere besides this machine: it is connected to a
/// remote, and the remote held everything recorded here when they last exchanged. Asks nothing
/// over the network, and makes no repository where none is.
pub fn notes_backed_up(root: String) -> Result<bool, String> {
    if remote_connection(root.clone())?.is_none() {
        return Ok(false);
    }
    match history_repo(Path::new(&root))? {
        Some(prepared) => {
            crate::remote::backed_up(prepared.repo()).map_err(|e| e.message().to_string())
        }
        // Nothing recorded yet: nothing that could be lost.
        None => Ok(true),
    }
}

/// Forgets this folder's remote on this machine. Nothing on the remote changes.
pub fn disconnect_remote(root: String) -> Result<(), String> {
    let root_p = Path::new(&root);
    let path = personal_dir(root_p)?.join(crate::remote::CONNECTION_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    crate::remote::clear_secret(&folder_key(root_p))
}

/// One exchange with this folder's remote: takes in what arrived where it can, and sends what
/// was recorded here.
pub fn sync_remote(root: String, locks: &NoteLocks) -> Result<crate::remote::Exchange, String> {
    let root_p = Path::new(&root);
    let connection = remote_connection(root.clone())?
        .ok_or_else(|| "this folder is not connected to a remote".to_string())?;
    let credentials = crate::remote::secret(&folder_key(root_p)).map(|secret| {
        crate::git_transport::Credentials {
            username: connection.username.clone(),
            secret,
        }
    });
    let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
    crate::remote::exchange(&prepared, root_p, &connection.url, credentials, locks)
}

/// Opens the OS app log directory in the system file explorer. Useful for diagnostic sharing.
/// Creates the directory if it does not yet exist (e.g. before the first app run that writes a log).
#[tauri::command]
pub fn open_log_dir(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    log::info!("open_log_dir: {}", dir.display());
    tauri_plugin_opener::open_path(dir.to_string_lossy().as_ref(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// The files `note_rel` links to by path that are inside the folder and on disk — the images and
/// attachments a version of the note shows. Notes, hidden files and web addresses are left out.
fn linked_files(root: &Path, note_rel: &str, body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for target in crate::note_refs::targets(&String::from_utf8_lossy(body)) {
        let crate::note_refs::Target::Path(t) = target else {
            continue;
        };
        let Some(rel) = crate::note_refs::resolve(note_rel, &t) else {
            continue;
        };
        if is_note_path(&rel)
            || rel.split('/').any(|s| s.starts_with('.'))
            || out
                .iter()
                .any(|seen: &String| seen.eq_ignore_ascii_case(&rel))
        {
            continue;
        }
        let mut path = root.to_path_buf();
        path.extend(rel.split('/'));
        if path.is_file() {
            out.push(rel);
        }
    }
    out
}

/// Records the given notes as one revision in the vault's history.
///
/// Only the paths named here are written. What the person working in the repository has
/// staged, which branch they have checked out, and what is in their working tree are all left
/// exactly as they were.
///
/// Returns `None` when the notes are already in the state history holds: nothing was added, and
/// saying so is the honest answer. Handing back the previous revision instead would read as a
/// new one that the history it belongs to does not list.
///
/// Refused rather than half-done when: the repository is in the middle of another operation,
/// a path lies outside the vault, or a path is covered by an ignore rule (in which case the
/// history would silently not contain what was asked for).
pub fn commit_notes(
    root: String,
    paths: Vec<String>,
    message: String,
) -> Result<Option<String>, String> {
    let root_p = Path::new(&root);
    if paths.is_empty() {
        return Err("nothing was selected".into());
    }

    let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
    let repo = prepared.repo();

    if let Some(state) = crate::git_engine::operation_in_progress(repo) {
        log::warn!("commit_notes: refused, repository is busy: {state:?}");
        return Err(format!(
            "the repository is in the middle of another operation ({state:?})"
        ));
    }

    let mut entries: Vec<(PathBuf, Vec<u8>)> = Vec::with_capacity(paths.len());
    for raw in &paths {
        let target = Path::new(raw);
        let rel = crate::vault_paths::rel_out(root_p, target)?;
        if target.is_dir() {
            // Reading a directory fails with an operating-system error that says nothing about
            // what went wrong. The caller expands a selection into files before asking.
            return Err(format!("'{rel}' is a folder, not a note"));
        }
        let in_repo = prepared.path_in_repo(Path::new(&rel));
        if repo.is_path_ignored(&in_repo).unwrap_or(false) {
            log::warn!("commit_notes: refused, path is ignored: {rel}");
            return Err(format!(
                "'{rel}' is covered by an ignore rule, so recording it would leave it out"
            ));
        }
        let content = std::fs::read(target).map_err(|e| e.to_string())?;
        // Only a note brings the files it links to; any other path is recorded as given.
        let linked = if is_note_path(&rel) {
            linked_files(root_p, &rel, &content)
        } else {
            Vec::new()
        };
        entries.push((in_repo, content));
        for file in linked {
            let file_in_repo = prepared.path_in_repo(Path::new(&file));
            let named = file_in_repo.to_string_lossy();
            if repo.is_path_ignored(&file_in_repo).unwrap_or(false)
                || entries
                    .iter()
                    .any(|(p, _)| p.to_string_lossy().eq_ignore_ascii_case(&named))
            {
                continue;
            }
            // A link may lead out of the folder through a junction or symlink.
            let Ok(path) = crate::vault_paths::resolve(&root, &file) else {
                log::warn!("commit_notes: linked file is outside the vault, left out: {file}");
                continue;
            };
            // Too large to publish, so not worth a copy in the history either: looked at before
            // anything is read.
            if std::fs::metadata(&path)
                .is_ok_and(|m| m.len() > crate::publish_assembly::MAX_LINKED_FILE_BYTES)
            {
                log::warn!("commit_notes: linked file over the size limit, left out: {file}");
                continue;
            }
            // A linked file is a companion: one that cannot be read must not stop the note.
            match crate::fs_ops::patiently(|| std::fs::read(&path)) {
                Ok(bytes) => entries.push((file_in_repo, bytes)),
                Err(e) => log::warn!("commit_notes: linked file left out, cannot read {file}: {e}"),
            }
        }
    }

    let (author, committer) =
        crate::git_engine::commit_identities(repo).map_err(|e| e.message().to_string())?;
    let oid = crate::git_engine::commit_paths(
        repo,
        crate::git_engine::NOTES_REF,
        &entries,
        &message,
        &author,
        &committer,
        crate::git_engine::WhenUnchanged::Skip,
    )
    .map_err(|e| e.message().to_string())?;

    match oid {
        Some(id) => {
            log::info!("commit_notes: {} path(s) as {}", entries.len(), id);
            Ok(Some(id.to_string()))
        }
        None => {
            log::info!("commit_notes: {} path(s) already held", entries.len());
            Ok(None)
        }
    }
}

/// One recorded state of a note, as the interface presents it.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NoteVersion {
    /// Opaque handle; hand it back to `note_version_text`.
    pub id: String,
    pub message: String,
    /// Unix epoch seconds.
    pub seconds: i64,
    pub author: String,
}

/// A note that history holds but the folder no longer does.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeletedNote {
    /// Vault-root-relative, `/`-separated.
    pub rel: String,
    /// Unix epoch seconds of when it left the folder.
    ///
    /// Taken from what was kept on the way out, which is written at that moment. For a note
    /// that left some other way — removed outside the application, or before its contents were
    /// being kept — nothing recorded the moment, and the last time it was written to history
    /// stands in as the closest thing known.
    pub seconds: i64,
    /// Whether it was ever recorded deliberately, as opposed to only kept when deleted.
    pub recorded: bool,
}

/// Where a restored note's contents came from.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoredNote {
    /// Vault-root-relative, `/`-separated path the note actually landed at.
    pub rel: String,
    /// True when the contents are the ones the note held at the moment it left the folder,
    /// false when they come from the last state recorded before that.
    ///
    /// The two differ whenever a note was edited without being recorded again, which is the
    /// case where saying which one came back matters most.
    pub as_deleted: bool,
}

/// The repository a folder's history lives in, without making one.
///
/// `Ok(None)` is the ordinary answer for a folder nothing has been recorded in. Looking at what
/// was recorded is not recording, and it must not turn a plain folder into a repository.
fn history_repo(root: &Path) -> Result<Option<crate::git_engine::VaultRepo>, String> {
    crate::git_engine::open_existing(root).map_err(|e| e.message().to_string())
}

/// Every recorded state of one note, newest first.
///
/// The note does not have to still be in the folder: this is how a deleted one is looked at
/// before deciding whether to bring it back.
pub fn note_versions(root: String, path: String) -> Result<Vec<NoteVersion>, String> {
    let root_p = Path::new(&root);
    let rel = crate::vault_paths::rel_out(root_p, &path)?;
    let Some(prepared) = history_repo(root_p)? else {
        return Ok(Vec::new());
    };
    let in_repo = prepared.path_in_repo(Path::new(&rel));
    let found = crate::git_engine::history(prepared.repo(), crate::git_engine::NOTES_REF, &in_repo)
        .map_err(|e| e.message().to_string())?;
    Ok(found
        .into_iter()
        .map(|v| NoteVersion {
            id: v.id,
            message: v.message,
            seconds: v.seconds,
            author: v.author,
        })
        .collect())
}

/// What a note held at one recorded state. Reads objects only — the file on disk is not touched.
pub fn note_version_text(root: String, path: String, id: String) -> Result<String, String> {
    let root_p = Path::new(&root);
    let rel = crate::vault_paths::rel_out(root_p, &path)?;
    let prepared = history_repo(root_p)?
        .ok_or_else(|| format!("'{rel}' is not part of that recorded state"))?;
    let in_repo = prepared.path_in_repo(Path::new(&rel));
    let content = crate::git_engine::content_at_version(
        prepared.repo(),
        crate::git_engine::NOTES_REF,
        &in_repo,
        &id,
    )
    .map_err(|e| e.message().to_string())?
    .ok_or_else(|| format!("'{rel}' is not part of that recorded state"))?;
    String::from_utf8(content).map_err(|_| "this state is not text".to_string())
}

/// Everything history holds that is no longer in the folder.
///
/// Two things end up here and the difference does not matter to whoever is looking for what
/// they deleted: notes that were recorded and later removed, and notes that were never recorded
/// but whose contents were kept when they were deleted.
pub fn deleted_notes(root: String) -> Result<Vec<DeletedNote>, String> {
    let root_p = Path::new(&root);
    let Some(prepared) = history_repo(root_p)? else {
        return Ok(Vec::new());
    };
    let repo = prepared.repo();

    let recorded_paths: std::collections::HashSet<String> =
        crate::git_engine::tip_paths(repo, crate::git_engine::NOTES_REF)
            .map_err(|e| e.message().to_string())?
            .into_iter()
            .collect();

    // Which notes are missing is settled first, from the folder alone. Only then is the history
    // asked anything, and only about those — the ones still in the folder have no question to
    // answer, and there are usually far more of them.
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut present = crate::fs_ops::PresentFiles::default();
    for in_repo in recorded_paths.iter().cloned().chain(
        crate::git_engine::tip_paths(repo, crate::git_engine::SNAPSHOT_REF)
            .map_err(|e| e.message().to_string())?,
    ) {
        let Some(rel) = prepared.path_in_vault(Path::new(&in_repo)) else {
            // Something the enclosing repository holds outside this folder.
            continue;
        };
        if present.contains(&root_p.join(&rel)) {
            continue;
        }
        missing.push((in_repo, rel.to_string_lossy().replace('\\', "/")));
    }
    // Deduplicated, because a note that was recorded and then deleted is named by both references
    // and would otherwise be asked about twice. The history walk stops once it has an answer for
    // everything asked, and a list that can never be satisfied — two entries, one answer — stops
    // only at the end of the history, which is the cost this whole path exists to avoid.
    let asking: Vec<String> = missing
        .iter()
        .map(|(in_repo, _)| in_repo.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let kept_times =
        crate::git_engine::last_written(repo, crate::git_engine::SNAPSHOT_REF, &asking)
            .map_err(|e| e.message().to_string())?;
    let recorded_times =
        crate::git_engine::last_written(repo, crate::git_engine::NOTES_REF, &asking)
            .map_err(|e| e.message().to_string())?;

    let mut seen: std::collections::HashMap<String, DeletedNote> = std::collections::HashMap::new();
    for (in_repo, rel) in missing {
        // When it left, not when it was last saved. Only the first of those answers the
        // question the list is sorted by.
        let seconds = kept_times
            .get(&in_repo)
            .or_else(|| recorded_times.get(&in_repo))
            .copied()
            .unwrap_or(0);
        seen.insert(
            rel.clone(),
            DeletedNote {
                rel,
                seconds,
                recorded: recorded_paths.contains(&in_repo),
            },
        );
    }

    let mut out: Vec<DeletedNote> = seen.into_values().collect();
    // Newest first, then by name so equal timestamps do not shuffle between calls.
    out.sort_by(|a, b| b.seconds.cmp(&a.seconds).then_with(|| a.rel.cmp(&b.rel)));
    Ok(out)
}

/// When `reference` last wrote one path, in seconds since the epoch.
fn last_written_at(
    repo: &git2::Repository,
    reference: &str,
    in_repo: &Path,
) -> Result<Option<i64>, String> {
    let asking = [in_repo.to_string_lossy().replace('\\', "/")];
    Ok(crate::git_engine::last_written(repo, reference, &asking)
        .map_err(|e| e.message().to_string())?
        .into_values()
        .next())
}

/// What a folder gave up when the application stopped keeping things inside it.
#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MoveOut {
    /// Whether settings — favourites, ordering, saved views — were carried over to where they
    /// live now.
    pub settings: bool,
    /// How many notes the set-aside copies held, now reachable as deleted notes.
    pub notes: usize,
    /// How many other files (attachments kept beside a deleted note) came with them.
    pub files: usize,
    /// What is still inside the folder's `.textree/` afterwards, relative to the folder.
    ///
    /// Anything here could not be carried — unreadable, refused by the repository, or not ours
    /// to know about — and stays exactly where it was. It is reported so that nothing tells the
    /// person their folder holds only notes while it does not.
    pub left_behind: Vec<String>,
}

impl MoveOut {
    fn happened(&self) -> bool {
        self.settings || self.notes > 0 || self.files > 0 || !self.left_behind.is_empty()
    }
}

/// What a walk under a directory found.
#[derive(Default)]
struct Beneath {
    /// Every file, paired with its path relative to the directory walked.
    files: Vec<(PathBuf, PathBuf)>,
    /// Directories whose contents could not be listed. Whatever they hold was not seen, so it
    /// can be neither carried nor removed.
    unlisted: Vec<PathBuf>,
}

/// Every file under `dir`, and every directory under it that could not be listed.
///
/// The second half is the guard: a directory that could not be read looks exactly like an empty
/// one to a walk that drops the failure, and an empty-looking directory is one that gets removed.
fn walk_beneath(dir: &Path) -> Beneath {
    let mut found = Beneath::default();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.is_dir() {
            match std::fs::read_dir(&current) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => stack.push(entry.path()),
                            Err(_) => found.unlisted.push(current.clone()),
                        }
                    }
                }
                Err(_) => found.unlisted.push(current.clone()),
            }
            continue;
        }
        if let Ok(rel) = current.strip_prefix(dir) {
            found.files.push((current.clone(), rel.to_path_buf()));
        }
    }
    found.unlisted.dedup();
    found
}

/// Removes every directory under `dir` (and `dir` itself) that holds nothing, deepest first.
///
/// Only ever removes a directory that is empty at the moment it is removed — `remove_dir` refuses
/// anything else — so nothing that holds a file, readable or not, can go this way.
fn prune_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                prune_empty_dirs(&path);
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Where an earlier copy of a path goes when a later copy of the same path is also carried.
///
/// Each copy the earlier version set aside could be restored on its own. Carried under one path,
/// only the latest would stay reachable, so the earlier ones are given the name the earlier
/// version's own trash gave a second copy — `name (1).md`, then `(2)` — skipping any name
/// already in the folder, already given in this pass, or already `occupied` in the history.
fn earlier_copy_name(
    root: &Path,
    rel: &str,
    given: &mut HashSet<String>,
    occupied: &dyn Fn(&str) -> bool,
) -> String {
    let path = Path::new(rel);
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut n = 1;
    loop {
        let candidate_name = format!("{stem} ({n}){ext}");
        let candidate = match parent {
            Some(p) => format!(
                "{}/{}",
                p.to_string_lossy().replace('\\', "/"),
                candidate_name
            ),
            None => candidate_name,
        };
        if !root.join(&candidate).exists()
            && !occupied(&candidate)
            && given.insert(candidate.to_lowercase())
        {
            return candidate;
        }
        n += 1;
    }
}

/// Every copy an earlier version set aside, with where it came from and when it left.
///
/// What is actually in the folder, which is not the same as what the list says is in there. A
/// corrupt list reads as empty, an entry can name a file that is gone, and a copy can be sitting
/// there with no entry at all. The earlier version treated that last case as ordinary — a copy
/// whose entry could not be written was kept, and its restore screen offered it back as being
/// of unknown origin, to the top of the folder. So does this: such a copy is carried under its
/// own name at the top of the folder, dated by when the file was last written, which is the
/// nearest thing to when it left that survives.
fn set_aside_copies(root: &Path, dir: &Path) -> Vec<TrashItem> {
    let mut on_disk: BTreeSet<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();

    let mut found = Vec::new();
    for mut item in read_trash_manifest(root) {
        // Listed twice, or listed but gone: the copy on disk is what counts, once.
        if !on_disk.remove(&item.trash_name) {
            continue;
        }
        // An entry whose recorded origin cannot be a path inside the folder still names a real
        // copy; it goes back the way an unlisted one does rather than being left behind.
        if validate_vault_rel(&item.original_rel).is_err() {
            item.original_rel = item.trash_name.clone();
        }
        found.push(item);
    }
    for name in on_disk {
        let at = dir.join(&name);
        let deleted_at = std::fs::metadata(&at)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        found.push(TrashItem {
            is_dir: at.is_dir(),
            original_rel: name.clone(),
            trash_name: name,
            deleted_at,
        });
    }
    found
}

/// Carries the copies an earlier version set aside into the history, dated as they were.
///
/// The date matters as much as the contents: the list of deleted notes is ordered by when each
/// one left, and the copies are the only place that answer exists for anything deleted before
/// the history started keeping it. Losing it here would silently reorder someone's list.
///
/// A file leaves the folder only once the history holds it, and only the files actually read go:
/// a directory is never removed wholesale, because whatever inside it could not be listed or read
/// has no other copy. Directories left holding nothing are then removed. So running this again
/// carries nothing twice, and whatever stayed is read off the folder afterwards rather than
/// reported from here.
fn carry_over_set_aside(root: &Path, prepared: &crate::git_engine::VaultRepo) -> CarriedOver {
    let dir = root.join(".textree").join("trash");
    let manifest = legacy_sidecar_path(root, TRASH_MANIFEST);
    let mut carried = CarriedOver::default();
    if !dir.exists() {
        // A list with no copies beside it describes nothing that exists.
        let _ = std::fs::remove_file(&manifest);
        return carried;
    }

    let repo = prepared.repo();
    let Ok((author, committer)) = crate::git_engine::commit_identities(repo) else {
        log::warn!("carry_over_set_aside: no identity available");
        return carried;
    };

    // Oldest first, so the newest write for any one path is also the latest deletion of it.
    let mut items = set_aside_copies(root, &dir);
    items.sort_by_key(|item| item.deleted_at);

    // A path deleted more than once left a copy each time. The latest keeps the path; each
    // earlier one is carried under a name of its own, so every copy stays reachable.
    let mut latest_of: HashMap<String, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        latest_of.insert(item.original_rel.to_lowercase(), i);
    }
    let mut given: HashSet<String> = latest_of.keys().cloned().collect();

    // What the history already holds for deleted content — carried on an earlier open (a copy
    // that could not be read then stays behind and arrives later), or deleted since. Writing a
    // copy over such a path would leave only the newest write reachable, so a path already held
    // is never written over: the copy arriving now takes a name of its own instead.
    let held: HashSet<String> = crate::git_engine::tip_paths(repo, crate::git_engine::SNAPSHOT_REF)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_lowercase())
        .collect();
    let occupied = |rel: &str| {
        let in_repo = prepared
            .path_in_repo(Path::new(rel))
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        let beneath = format!("{in_repo}/");
        held.contains(&in_repo) || held.iter().any(|p| p.starts_with(&beneath))
    };

    for (i, item) in items.iter().enumerate() {
        if validate_vault_rel(&item.original_rel).is_err() {
            // Not a name that can exist inside the folder (a dot-name, a reserved device name):
            // it stays, and the report says so.
            continue;
        }
        let latest = latest_of.get(&item.original_rel.to_lowercase()) == Some(&i);
        let carried_as = if latest && !occupied(&item.original_rel) {
            item.original_rel.clone()
        } else {
            earlier_copy_name(root, &item.original_rel, &mut given, &occupied)
        };
        let put_aside = dir.join(&item.trash_name);
        let sources = if item.is_dir {
            walk_beneath(&put_aside).files
        } else {
            vec![(put_aside.clone(), PathBuf::new())]
        };
        let mut entries: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        let mut read_from: Vec<PathBuf> = Vec::new();
        for (on_disk, below) in sources {
            let Ok(content) = std::fs::read(&on_disk) else {
                continue;
            };
            let mut target = PathBuf::from(&carried_as);
            if !below.as_os_str().is_empty() {
                target = target.join(&below);
            }
            entries.push((prepared.path_in_repo(&target), content));
            read_from.push(on_disk);
        }
        if entries.is_empty() {
            // Nothing readable in it. An empty directory holds nothing and goes; one with
            // anything unreadable inside keeps all of it.
            if item.is_dir {
                prune_empty_dirs(&put_aside);
            }
            continue;
        }
        let when = git2::Time::new(item.deleted_at as i64, 0);
        let dated = |who: &git2::Signature<'_>| {
            git2::Signature::new(
                who.name().unwrap_or("Textree"),
                who.email().unwrap_or("noreply@textree.me"),
                &when,
            )
        };
        let (Ok(author_then), Ok(committer_then)) = (dated(&author), dated(&committer)) else {
            continue;
        };
        match crate::git_engine::commit_paths(
            repo,
            crate::git_engine::SNAPSHOT_REF,
            &entries,
            "keep deleted content",
            &author_then,
            &committer_then,
            crate::git_engine::WhenUnchanged::Record,
        ) {
            Ok(_) => {
                for (rel, _) in &entries {
                    let is_note = rel
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("md"));
                    if is_note {
                        carried.notes += 1;
                    } else {
                        carried.files += 1;
                    }
                }
                // Held by the history now, file by file — and only the files that were read.
                for on_disk in &read_from {
                    if let Err(e) = crate::fs_ops::patiently(|| std::fs::remove_file(on_disk)) {
                        log::warn!(
                            "carry_over_set_aside: {} carried but not removed: {e}",
                            on_disk.display()
                        );
                    }
                }
                if item.is_dir {
                    prune_empty_dirs(&put_aside);
                }
            }
            Err(e) => log::warn!(
                "carry_over_set_aside: {} not carried: {}",
                item.original_rel,
                e.message()
            ),
        }
    }

    // The list only described the copies. Once none are left it describes nothing.
    prune_empty_dirs(&dir);
    if !dir.exists() {
        let _ = std::fs::remove_file(&manifest);
    }
    carried
}

/// What came out of the set-aside copies.
#[derive(Default)]
struct CarriedOver {
    notes: usize,
    files: usize,
}

/// Everything still under `dir`, relative to `root`, `/`-separated, in a stable order: every file,
/// and every directory whose contents could not be listed. Never empty while `dir` exists — a
/// directory that is still there is itself something left behind.
fn still_inside(root: &Path, dir: &Path) -> Vec<String> {
    let rel = |abs: &Path| {
        abs.strip_prefix(root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    };
    let found = walk_beneath(dir);
    let mut left: Vec<String> = found
        .files
        .iter()
        .map(|(abs, _)| abs.as_path())
        .chain(found.unlisted.iter().map(|p| p.as_path()))
        .filter_map(rel)
        .collect();
    if left.is_empty() && dir.exists() {
        left.extend(rel(dir));
    }
    left.sort();
    left.dedup();
    left
}

/// Moves everything the application keeps out of the notes folder, keeping every file.
///
/// Run when a folder is opened, and safe to run again: a folder that has nothing left inside it
/// reports that nothing happened.
///
/// The copies set aside for deleted notes go into the history rather than being thrown away —
/// they are still the only copy of anything deleted before the history started keeping them.
/// Settings go where settings live now. Only then is the empty directory removed.
pub fn move_state_out_of_vault(root: String) -> Result<MoveOut, String> {
    let root_p = Path::new(&root);
    let inside = root_p.join(".textree");
    if !inside.exists() {
        return Ok(MoveOut::default());
    }
    let mut moved = MoveOut::default();

    // Settings first: cheap, and it cannot fail in a way that costs anything.
    // Every settings file the application has ever kept in there. A name missing from this list
    // is a file left behind in a folder the notice claims is clean.
    //
    // Not into settings a newer release wrote, though: it may keep them in another shape, and these
    // are in the old one. They stay where they are, and the notice lists them as left behind.
    let settings_writable =
        crate::state_dir::writable(&personal_dir(root_p)?).map_err(|e| e.to_string())?;
    for rel in ["favorites.json", "order.json", "views.json"] {
        if !settings_writable {
            break;
        }
        let from = legacy_sidecar_path(root_p, rel);
        if !from.is_file() {
            continue;
        }
        let to = sidecar_path(root_p, rel)?;
        if to.exists() {
            // Already carried over on an earlier run; the copy left behind is the stale one.
            let _ = std::fs::remove_file(&from);
            continue;
        }
        // Read as bytes and carried across unchanged. Insisting on valid text here would abort
        // the whole move over one damaged file, and the abort is not where it ends: the next
        // write puts a fresh file at the new location, and the move after that sees one there
        // and removes the old one — so the settings that could not be read get discarded by the
        // recovery rather than by anything that decided to.
        let content = std::fs::read(&from).map_err(|e| e.to_string())?;
        atomic_bytes_beside(&to, &content).map_err(|e| e.to_string())?;
        crate::fs_ops::patiently(|| std::fs::remove_file(&from)).map_err(|e| e.to_string())?;
        moved.settings = true;
    }
    if moved.settings {
        // Settings kept inside the folder name notes the old way, whatever format the settings
        // folder they land in is at — bringing that folder up to date does not run again for them.
        crate::state_dir::relative_names_in(&personal_dir(root_p)?, &root, &atomic_bytes_beside)
            .map_err(|e| e.to_string())?;
    }

    // The copies set aside need somewhere to go before they can be removed from the folder.
    // Each leaves the folder only once the history holds it: a copy that failed to carry has no
    // other copy by definition — that is what being set aside meant.
    if !root_p.join(".textree").join("trash").exists() {
        // A list with no copies beside it describes nothing that exists — and there is nothing to
        // carry, so no repository is made for it.
        let _ = std::fs::remove_file(legacy_sidecar_path(root_p, TRASH_MANIFEST));
    } else {
        let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
        let carried = carry_over_set_aside(root_p, &prepared);
        moved.notes = carried.notes;
        moved.files = carried.files;
    }

    // Temp files are the application's own and hold nothing a person made, so they go. The
    // directory itself goes only once it holds nothing — and whatever it still holds is read off
    // the disk and reported, so the notice cannot say the folder is clean while it is not.
    let _ = std::fs::remove_dir_all(root_p.join(LEGACY_TEMP_DIR[0]).join(LEGACY_TEMP_DIR[1]));
    prune_empty_dirs(&inside);
    if inside.exists() {
        moved.left_behind = still_inside(root_p, &inside);
    }

    if moved.happened() {
        log::info!(
            "move_state_out_of_vault: settings={} notes={} files={} left_behind={}",
            moved.settings,
            moved.notes,
            moved.files,
            moved.left_behind.len()
        );
    }
    Ok(moved)
}

/// Puts a note back to one of its recorded states.
///
/// What is on disk right now is kept first. Going back is the one action here that overwrites
/// work rather than adding to it, and work that was never recorded has nowhere else to be —
/// so the state being replaced is put where the deleted ones go, and stays reachable.
///
/// The note has to still be in the folder: bringing back one that is not is `restore_deleted`,
/// which decides for itself which state that should be.
pub fn restore_version(root: String, path: String, id: String) -> Result<(), String> {
    let root_p = Path::new(&root);
    let target = Path::new(&path);
    let rel = crate::vault_paths::rel_out(root_p, target)?;
    let prepared = history_repo(root_p)?
        .ok_or_else(|| format!("'{rel}' is not part of that recorded state"))?;
    let in_repo = prepared.path_in_repo(Path::new(&rel));
    let content = crate::git_engine::content_at_version(
        prepared.repo(),
        crate::git_engine::NOTES_REF,
        &in_repo,
        &id,
    )
    .map_err(|e| e.message().to_string())?
    .ok_or_else(|| format!("'{rel}' is not part of that recorded state"))?;

    keep_state_of(root_p, target)?;
    atomic_write_bytes(root_p, target, &content).map_err(|e| e.to_string())?;
    log::info!("restore_version: {rel} at {id}");
    Ok(())
}

/// One open alternative of a note, as the interface presents it.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NoteAlternative {
    /// Opaque handle; hand it back to the other alternative commands.
    pub id: String,
    /// Vault-root-relative, `/`-separated path of the note it is an alternative of.
    pub rel: String,
    /// True when it arrived from elsewhere: the note was changed both here and there.
    pub arrived: bool,
    /// Unix epoch seconds of its newest version.
    pub seconds: i64,
    pub author: String,
    /// False while it holds the same as the note's last recorded version — nothing to choose yet.
    pub differs: bool,
}

fn note_alternative(
    prepared: &crate::git_engine::VaultRepo,
    alternative: &crate::alternatives::Alternative,
) -> Result<Option<NoteAlternative>, String> {
    // An enclosing repository may hold alternatives of notes in other folders.
    let Some(rel) = prepared.path_in_vault(Path::new(&alternative.path)) else {
        return Ok(None);
    };
    let differs = crate::alternatives::differs(prepared.repo(), alternative)
        .map_err(|e| e.message().to_string())?;
    Ok(Some(NoteAlternative {
        id: alternative.id.clone(),
        rel: crate::git_engine::slashed(&rel),
        arrived: alternative.arrived_from.is_some(),
        seconds: alternative.seconds,
        author: alternative.author.clone(),
        differs,
    }))
}

/// The repository of a folder something has been recorded in, refusing while it is busy.
fn writable_history(root: &Path) -> Result<crate::git_engine::VaultRepo, String> {
    let prepared =
        history_repo(root)?.ok_or_else(|| "add a version of this note first".to_string())?;
    if let Some(state) = crate::git_engine::operation_in_progress(prepared.repo()) {
        return Err(format!(
            "the repository is in the middle of another operation ({state:?})"
        ));
    }
    Ok(prepared)
}

/// Every open alternative of the notes in this folder.
pub fn list_alternatives(root: String) -> Result<Vec<NoteAlternative>, String> {
    let Some(prepared) = history_repo(Path::new(&root))? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for alternative in
        crate::alternatives::list(prepared.repo()).map_err(|e| e.message().to_string())?
    {
        if let Some(view) = note_alternative(&prepared, &alternative)? {
            out.push(view);
        }
    }
    Ok(out)
}

/// Starts an alternative of a note from its last recorded version.
pub fn start_alternative(root: String, path: String) -> Result<NoteAlternative, String> {
    let root_p = Path::new(&root);
    let rel = crate::vault_paths::rel_out(root_p, &path)?;
    let prepared = writable_history(root_p)?;
    let repo = prepared.repo();
    let (author, committer) =
        crate::git_engine::commit_identities(repo).map_err(|e| e.message().to_string())?;
    let id = crate::alternatives::start(
        repo,
        &prepared.path_in_repo(Path::new(&rel)),
        &author,
        &committer,
    )
    .map_err(|e| e.message().to_string())?;
    let alternative = crate::alternatives::get(repo, &id).map_err(|e| e.message().to_string())?;
    log::info!("start_alternative: {rel} as {id}");
    note_alternative(&prepared, &alternative)?
        .ok_or_else(|| "the note is not in this folder".into())
}

/// What an alternative holds for its note.
pub fn alternative_text(root: String, id: String) -> Result<String, String> {
    let prepared =
        history_repo(Path::new(&root))?.ok_or_else(|| "no such alternative".to_string())?;
    let repo = prepared.repo();
    let alternative = crate::alternatives::get(repo, &id).map_err(|e| e.message().to_string())?;
    let bytes = crate::alternatives::content(repo, &alternative)
        .map_err(|e| e.message().to_string())?
        .unwrap_or_default();
    String::from_utf8(bytes).map_err(|_| "the alternative is not text".into())
}

/// Adds a version to an alternative. `None` when it already holds exactly this.
pub fn add_alternative_version(
    root: String,
    id: String,
    text: String,
    message: String,
) -> Result<Option<String>, String> {
    let prepared = writable_history(Path::new(&root))?;
    let repo = prepared.repo();
    let (author, committer) =
        crate::git_engine::commit_identities(repo).map_err(|e| e.message().to_string())?;
    let _turn = crate::drafts::TURN
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let oid =
        crate::alternatives::record(repo, &id, text.as_bytes(), &message, &author, &committer)
            .map_err(|e| e.message().to_string())?;
    // The draft is now a version — unless something newer was typed since this text was taken.
    let alternative = crate::alternatives::get(repo, &id).map_err(|e| e.message().to_string())?;
    let file = draft_file(&drafts_dir(Path::new(&root))?, &alternative)?;
    if crate::drafts::read(&file)
        .map_err(|e| e.to_string())?
        .as_deref()
        == Some(text.as_str())
    {
        forget_draft(&drafts_dir(Path::new(&root))?, &id);
    }
    Ok(oid.map(|o| o.to_string()))
}

/// Makes an alternative's version the note: written to the file and recorded as one new version,
/// and the alternative ends.
///
/// What is on disk is kept first, as when going back to an earlier version: picking overwrites
/// the note, and edits never recorded have nowhere else to be.
pub fn use_alternative(root: String, path: String, id: String) -> Result<(), String> {
    let root_p = Path::new(&root);
    let target = Path::new(&path);
    let rel = crate::vault_paths::rel_out(root_p, target)?;
    let prepared = writable_history(root_p)?;
    let repo = prepared.repo();
    let alternative = crate::alternatives::get(repo, &id).map_err(|e| e.message().to_string())?;
    if alternative.path != crate::git_engine::slashed(&prepared.path_in_repo(Path::new(&rel))) {
        return Err("that alternative belongs to another note".into());
    }
    let _turn = crate::drafts::TURN
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let drafts = drafts_dir(root_p)?;
    add_draft_as_version(repo, &drafts, &alternative)?;
    let alternative = crate::alternatives::get(repo, &id).map_err(|e| e.message().to_string())?;
    let chosen = crate::alternatives::content(repo, &alternative)
        .map_err(|e| e.message().to_string())?
        .ok_or_else(|| "that alternative holds no version of its note".to_string())?;

    keep_state_of(root_p, target)?;
    atomic_write_bytes(root_p, target, &chosen).map_err(|e| e.to_string())?;
    let (author, committer) =
        crate::git_engine::commit_identities(repo).map_err(|e| e.message().to_string())?;
    crate::alternatives::pick(repo, &id, &author, &committer)
        .map_err(|e| e.message().to_string())?;
    forget_draft(&drafts, &id);
    log::info!("use_alternative: {rel} from {id}");
    Ok(())
}

/// Ends an alternative without using it. The note stays as it is; the alternative is kept.
///
/// What was still being typed into it is added to it as a version first, so the ended alternative
/// keeps it.
pub fn set_aside_alternative(root: String, id: String) -> Result<(), String> {
    let root_p = Path::new(&root);
    let prepared = writable_history(root_p)?;
    let repo = prepared.repo();
    let _turn = crate::drafts::TURN
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let drafts = drafts_dir(root_p)?;
    let alternative = crate::alternatives::get(repo, &id).map_err(|e| e.message().to_string())?;
    add_draft_as_version(repo, &drafts, &alternative)?;
    crate::alternatives::end(repo, &id).map_err(|e| e.message().to_string())?;
    forget_draft(&drafts, &id);
    log::info!("set_aside_alternative: {id}");
    Ok(())
}

fn drafts_dir(root: &Path) -> Result<PathBuf, String> {
    Ok(personal_dir(root)?.join(crate::drafts::DIR))
}

fn draft_file(
    drafts: &Path,
    alternative: &crate::alternatives::Alternative,
) -> Result<PathBuf, String> {
    crate::drafts::file(drafts, &alternative.id, &alternative.path)
        .ok_or_else(|| "that alternative's note cannot be kept here".to_string())
}

/// Adds what is being typed into `alternative` to it as a version, when there is anything.
fn add_draft_as_version(
    repo: &git2::Repository,
    drafts: &Path,
    alternative: &crate::alternatives::Alternative,
) -> Result<(), String> {
    let Some(text) =
        crate::drafts::read(&draft_file(drafts, alternative)?).map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let (author, committer) =
        crate::git_engine::commit_identities(repo).map_err(|e| e.message().to_string())?;
    crate::alternatives::record(
        repo,
        &alternative.id,
        text.as_bytes(),
        "Alternative version",
        &author,
        &committer,
    )
    .map_err(|e| e.message().to_string())?;
    Ok(())
}

/// The draft is in the ended alternative by now; one left behind costs nothing but space.
fn forget_draft(drafts: &Path, id: &str) {
    if let Err(e) = crate::drafts::remove(drafts, id) {
        log::warn!("alternative {id}: its draft could not be removed: {e}");
    }
}

/// What is being typed into an alternative, or `None` when nothing is — it then holds just its
/// last version (see [`alternative_text`]).
pub fn alternative_draft(root: String, id: String) -> Result<Option<String>, String> {
    let root_p = Path::new(&root);
    let prepared = history_repo(root_p)?.ok_or_else(|| "no such alternative".to_string())?;
    let alternative =
        crate::alternatives::get(prepared.repo(), &id).map_err(|e| e.message().to_string())?;
    crate::drafts::read(&draft_file(&drafts_dir(root_p)?, &alternative)?).map_err(|e| e.to_string())
}

/// Keeps what is being typed into an alternative, outside the folder. The note is not touched.
pub fn write_alternative_draft(root: String, id: String, text: String) -> Result<(), String> {
    let root_p = Path::new(&root);
    let prepared = history_repo(root_p)?.ok_or_else(|| "no such alternative".to_string())?;
    if !crate::state_dir::writable(&personal_dir(root_p)?).map_err(|e| e.to_string())? {
        return Err(NEWER_SETTINGS.into());
    }
    let _turn = crate::drafts::TURN
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let alternative =
        crate::alternatives::get(prepared.repo(), &id).map_err(|e| e.message().to_string())?;
    atomic_write_beside(&draft_file(&drafts_dir(root_p)?, &alternative)?, &text)
        .map_err(|e| e.to_string())
}

/// Brings a deleted note back into the folder.
///
/// Two places may hold it — what was kept when it left, and the last state recorded before
/// that — and **the newer of the two is the one brought back**. Preferring either place by rule
/// gets it wrong in one direction or the other: reading history first loses edits that were
/// never recorded, and reading what was kept first hands back a stale copy to a note that was
/// deleted, restored, saved, and then removed some other way.
///
/// An existing file of the same name is never overwritten: the restored copy is numbered
/// alongside it, and the path it actually landed at is returned so the caller can say where.
pub fn restore_deleted(root: String, rel: String) -> Result<RestoredNote, String> {
    let root_p = Path::new(&root);
    validate_vault_rel(&rel)?;
    let prepared =
        history_repo(root_p)?.ok_or_else(|| format!("'{rel}' is not in this folder's history"))?;
    let repo = prepared.repo();
    let in_repo = prepared.path_in_repo(Path::new(&rel));

    let kept = last_written_at(repo, crate::git_engine::SNAPSHOT_REF, &in_repo)?;
    let recorded = last_written_at(repo, crate::git_engine::NOTES_REF, &in_repo)?;
    // A tie goes to what was kept: it is written as the note leaves, so it is at or after
    // whatever was recorded in the same second.
    let as_deleted = match (kept, recorded) {
        (Some(k), Some(r)) => k >= r,
        (Some(_), None) => true,
        _ => false,
    };
    let reference = if as_deleted {
        crate::git_engine::SNAPSHOT_REF
    } else {
        crate::git_engine::NOTES_REF
    };

    let content = crate::git_engine::content_at_tip(repo, reference, &in_repo)
        .map_err(|e| e.message().to_string())?
        .ok_or_else(|| format!("'{rel}' is not in this folder's history"))?;

    // The outermost folder bringing it back will make, if its folder went away too.
    let mut made_folder = None;
    let mut above = root_p.join(&rel).parent().map(Path::to_path_buf);
    while let Some(dir) = above.filter(|d| !d.exists() && d.starts_with(root_p) && d != root_p) {
        above = dir.parent().map(Path::to_path_buf);
        made_folder = Some(dir);
    }

    // Found free, then taken before it is written (a sync client, another program): the next free
    // place instead — bringing something back never costs what is there.
    let first = crate::fs_ops::place_restored(root_p, &rel).map_err(|e| e.to_string())?;
    let (dir, name) = (
        first.parent().ok_or("no parent directory")?,
        Path::new(&rel)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("cannot read name")?,
    );
    let (dest, ()) =
        tauri_kit_fs::claim_free_path(dir, name, tauri_kit_fs::NameKind::File, |dest| {
            atomic_create_bytes(root_p, dest, &content)
        })
        .map_err(|e| e.to_string())?;
    log::info!(
        "restore_deleted: {} (as deleted: {as_deleted})",
        dest.display()
    );
    let landed = crate::vault_paths::rel_out(root_p, &dest)?;
    // Back as a note made there now: no include left there applies to it or to a folder made to
    // hold it; a leave-out still does.
    let mut made = vec![landed.clone()];
    made.extend(made_folder.and_then(|d| crate::vault_paths::rel_out(root_p, &d).ok()));
    forget_choices(root_p, &made);
    Ok(RestoredNote {
        rel: landed,
        as_deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_screen_gets_the_tree_with_paths_relative_to_the_folder() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("blog")).unwrap();
        std::fs::write(tmp.path().join("blog").join("blog.md"), "own").unwrap();
        std::fs::write(tmp.path().join("blog").join("post.md"), "x").unwrap();
        std::fs::write(tmp.path().join("top.md"), "x").unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let tree = tree_out(&root, tree_of(root.clone()).unwrap()).unwrap();
        let blog = tree.iter().find(|n| n.name == "blog").unwrap();
        assert_eq!(blog.path, "blog");
        assert_eq!(blog.body_path.as_deref(), Some("blog/blog.md"));
        assert_eq!(
            blog.children
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            ["blog/post.md"]
        );
        assert!(tree
            .iter()
            .any(|n| n.path == "top.md" && n.body_path.as_deref() == Some("top.md")));
    }

    #[test]
    fn a_site_without_a_title_is_named_after_the_notes_folder() {
        let none = crate::publish::PublishOptions {
            site_title: None,
            tokens_css: Some("x".into()),
        };
        let named = with_default_title(none, Path::new("/some/where/My Notes"));
        assert_eq!(named.site_title.as_deref(), Some("My Notes"));
        assert_eq!(named.tokens_css.as_deref(), Some("x"));
        let given = crate::publish::PublishOptions {
            site_title: Some("Mine".into()),
            tokens_css: None,
        };
        assert_eq!(
            with_default_title(given, Path::new("/some/where/My Notes"))
                .site_title
                .as_deref(),
            Some("Mine")
        );
    }

    #[test]
    fn stale_base_allows_a_save_while_the_note_still_holds_its_base() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("a.md");
        std::fs::write(
            &note, "loaded
",
        )
        .unwrap();
        assert_eq!(
            stale_base(
                &note, "loaded
"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn stale_base_refuses_a_save_over_a_change_made_elsewhere() {
        let tmp = TempDir::new().unwrap();
        let note = tmp.path().join("a.md");
        std::fs::write(&note, "changed elsewhere").unwrap();
        assert_eq!(
            stale_base(&note, "loaded").unwrap(),
            Some(WriteOutcome::Conflict {
                disk: Some("changed elsewhere".into())
            })
        );
    }

    #[test]
    fn stale_base_reports_a_note_that_is_gone() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            stale_base(&tmp.path().join("gone.md"), "loaded").unwrap(),
            Some(WriteOutcome::Conflict { disk: None })
        );
    }

    #[test]
    fn stale_base_reports_a_note_whose_folder_is_gone() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            stale_base(&tmp.path().join("box").join("gone.md"), "loaded").unwrap(),
            Some(WriteOutcome::Conflict { disk: None })
        );
    }

    /// Starts saving `text` to `path` on another thread and holds it just before the rename — the
    /// moment a stalled folder would hold it. Returns a way to let it go and its result.
    fn save_held_before_rename(
        root: &Path,
        path: &Path,
        text: &'static str,
        expected: &'static str,
        own: &tauri_kit_watch::OwnWrites,
        locks: &Arc<NoteLocks>,
    ) -> (
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<Result<WriteOutcome, String>>,
    ) {
        use std::sync::mpsc;
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let (root, path) = (root.to_path_buf(), path.to_path_buf());
        let (own, locks) = (own.clone(), Arc::clone(locks));
        let handle = std::thread::spawn(move || {
            let _hook = write_step::install(move |step| {
                if step == write_step::Step::Rename {
                    reached_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok(())
            });
            save_note(&root, &path, text, expected, &own, &locks)
        });
        reached
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the held save starts");
        (release, handle)
    }

    #[test]
    fn a_save_that_lands_late_cannot_overwrite_a_newer_one() {
        // The stalled save goes first and the newer one waits for it to finish, then checks the
        // note against what it left. Without that, the stalled save would land last and put the
        // older text back.
        let tmp = TempDir::new().unwrap();
        let (root, note) = (tmp.path(), tmp.path().join("a.md"));
        std::fs::write(&note, "0").unwrap();
        let (own, locks) = (
            tauri_kit_watch::OwnWrites::new(),
            Arc::new(NoteLocks::default()),
        );

        let (release, older) = save_held_before_rename(root, &note, "older", "0", &own, &locks);

        let newer = {
            let (root, note) = (root.to_path_buf(), note.clone());
            let (own, locks) = (own.clone(), Arc::clone(&locks));
            std::thread::spawn(move || save_note(&root, &note, "newer", "older", &own, &locks))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !newer.is_finished(),
            "the newer save waits while the older one is still writing"
        );
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "0");

        release.send(()).unwrap();
        assert_eq!(older.join().unwrap(), Ok(WriteOutcome::Written));
        assert_eq!(newer.join().unwrap(), Ok(WriteOutcome::Written));
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "newer",
            "the newest text is what stays"
        );
    }

    /// Renames `a.md` to `b.md` on another thread the way the command does: holding a turn over
    /// the note first, then renaming.
    fn rename_in_turn(
        root: &Path,
        locks: &Arc<NoteLocks>,
    ) -> std::thread::JoinHandle<Result<String, String>> {
        let (root, locks) = (root.to_path_buf(), Arc::clone(locks));
        std::thread::spawn(move || {
            let note = root.join("a.md");
            let _turn = locks.subtree(&[note.as_path()]);
            rename_node(
                root.to_string_lossy().into(),
                note.to_string_lossy().into(),
                "b".into(),
            )
        })
    }

    #[test]
    fn a_rename_waits_for_a_stalled_save_so_the_old_name_does_not_come_back() {
        let tmp = TempDir::new().unwrap();
        let (root, note) = (tmp.path(), tmp.path().join("a.md"));
        std::fs::write(&note, "0").unwrap();
        let (own, locks) = (
            tauri_kit_watch::OwnWrites::new(),
            Arc::new(NoteLocks::default()),
        );

        let (release, save) = save_held_before_rename(root, &note, "newest", "0", &own, &locks);
        let rename = rename_in_turn(root, &locks);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !rename.is_finished(),
            "the rename waits while a save to the note is still writing"
        );

        release.send(()).unwrap();
        assert_eq!(save.join().unwrap(), Ok(WriteOutcome::Written));
        rename.join().unwrap().unwrap();
        assert!(!note.exists(), "the old name must not come back");
        assert_eq!(
            std::fs::read_to_string(root.join("b.md")).unwrap(),
            "newest"
        );
    }

    #[test]
    fn negative_control_a_rename_without_a_turn_lets_a_late_save_bring_the_old_name_back() {
        let tmp = TempDir::new().unwrap();
        let (root, note) = (tmp.path(), tmp.path().join("a.md"));
        std::fs::write(&note, "0").unwrap();
        let (own, locks) = (
            tauri_kit_watch::OwnWrites::new(),
            Arc::new(NoteLocks::default()),
        );

        let (release, save) = save_held_before_rename(root, &note, "newest", "0", &own, &locks);
        let root_s = root.to_string_lossy().to_string();
        rename_node(root_s, note.to_string_lossy().into(), "b".into()).unwrap();
        release.send(()).unwrap();
        assert_eq!(save.join().unwrap(), Ok(WriteOutcome::Written));

        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "newest",
            "the old name came back"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.md")).unwrap(),
            "0",
            "without the edit"
        );
    }

    #[test]
    fn a_save_arriving_during_a_rename_is_told_the_note_is_gone() {
        // After the rename the note is elsewhere: a save still aimed at the old name must not
        // create it again, and reports it gone so the editor follows the move instead.
        let tmp = TempDir::new().unwrap();
        let (root, note) = (tmp.path(), tmp.path().join("a.md"));
        std::fs::write(&note, "0").unwrap();
        let (own, locks) = (
            tauri_kit_watch::OwnWrites::new(),
            Arc::new(NoteLocks::default()),
        );

        let turn = locks.subtree(&[note.as_path()]);
        let late = {
            let (root, note) = (root.to_path_buf(), note.clone());
            let (own, locks) = (own.clone(), Arc::clone(&locks));
            std::thread::spawn(move || save_note(&root, &note, "late", "0", &own, &locks))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !late.is_finished(),
            "a save to a note being renamed waits for the rename"
        );
        let root_s = root.to_string_lossy().to_string();
        rename_node(root_s, note.to_string_lossy().into(), "b".into()).unwrap();
        drop(turn);

        assert_eq!(
            late.join().unwrap(),
            Ok(WriteOutcome::Conflict { disk: None })
        );
        assert!(!note.exists());
        assert_eq!(std::fs::read_to_string(root.join("b.md")).unwrap(), "0");
    }

    #[test]
    fn a_stalled_save_does_not_hold_up_another_note() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.md"), "a").unwrap();
        std::fs::write(root.join("b.md"), "b").unwrap();
        let (own, locks) = (
            tauri_kit_watch::OwnWrites::new(),
            Arc::new(NoteLocks::default()),
        );

        let (release, stalled) =
            save_held_before_rename(root, &root.join("a.md"), "a2", "a", &own, &locks);

        let other = save_note(root, &root.join("b.md"), "b2", "b", &own, &locks);
        assert_eq!(other, Ok(WriteOutcome::Written));
        assert_eq!(std::fs::read_to_string(root.join("b.md")).unwrap(), "b2");

        release.send(()).unwrap();
        assert_eq!(stalled.join().unwrap(), Ok(WriteOutcome::Written));
    }

    #[test]
    fn negative_control_without_taking_turns_the_newer_save_is_refused() {
        // The same two saves with the order left to chance: the newer one checks the note before the
        // stalled one lands, sees what it expects to replace is not there yet, and refuses — so the
        // person is told of a conflict with their own edit. With turns, it goes after (see above).
        let tmp = TempDir::new().unwrap();
        let (root, note) = (tmp.path(), tmp.path().join("a.md"));
        std::fs::write(&note, "0").unwrap();
        let own = tauri_kit_watch::OwnWrites::new();
        // A fresh set of locks per save is no locking at all.
        let (release, older) = save_held_before_rename(
            root,
            &note,
            "older",
            "0",
            &own,
            &Arc::new(NoteLocks::default()),
        );
        let newer = save_note(root, &note, "newer", "older", &own, &NoteLocks::default());
        assert!(
            matches!(newer, Ok(WriteOutcome::Conflict { .. })),
            "{newer:?}"
        );
        release.send(()).unwrap();
        older.join().unwrap().unwrap();
    }

    #[test]
    fn opening_a_folder_marks_its_settings_with_the_current_format() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        assert_eq!(
            prepare_sidecar(root.clone()).unwrap(),
            SidecarState { read_only: false }
        );
        let marker = personal_dir(tmp.path())
            .unwrap()
            .join(crate::state_dir::FORMAT_FILE);
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            crate::state_dir::CURRENT.to_string()
        );
        write_sidecar(root.clone(), "favorites.json".into(), "[]".into()).unwrap();
    }

    #[test]
    fn settings_a_newer_release_saved_are_shown_but_never_written_over() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let dir = personal_dir(tmp.path()).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::state_dir::FORMAT_FILE), "99").unwrap();
        std::fs::write(dir.join("order.json"), b"{\"another\":\"shape\"}").unwrap();

        for _ in 0..2 {
            assert_eq!(
                prepare_sidecar(root.clone()).unwrap(),
                SidecarState { read_only: true }
            );
        }
        assert!(write_sidecar(root.clone(), "order.json".into(), "{}".into()).is_err());
        assert!(write_sidecar(root.clone(), "favorites.json".into(), "[]".into()).is_err());
        assert!(set_aside_sidecar(root.clone(), "order.json".into()).is_err());

        assert_eq!(
            std::fs::read(dir.join("order.json")).unwrap(),
            b"{\"another\":\"shape\"}"
        );
        assert!(!dir.join("favorites.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(crate::state_dir::FORMAT_FILE)).unwrap(),
            "99"
        );
    }

    #[test]
    fn settings_left_inside_the_folder_are_not_carried_into_ones_a_newer_release_saved() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let dir = personal_dir(tmp.path()).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::state_dir::FORMAT_FILE), "99").unwrap();
        let legacy = legacy_sidecar_path(tmp.path(), "favorites.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "[\"old.md\"]").unwrap();

        let moved = move_state_out_of_vault(root).unwrap();

        assert!(!dir.join("favorites.json").exists());
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "[\"old.md\"]");
        assert!(
            moved
                .left_behind
                .iter()
                .any(|p| p.ends_with("favorites.json")),
            "{:?}",
            moved.left_behind
        );
    }

    #[test]
    fn settings_left_inside_the_folder_are_still_carried_out_after_the_format_is_marked() {
        // The marker describes the settings kept outside the folder. What an older release on
        // another machine leaves inside a synced folder is carried out whatever the marker says.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        prepare_sidecar(root.clone()).unwrap();
        let legacy = legacy_sidecar_path(tmp.path(), "order.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "{}").unwrap();

        assert!(move_state_out_of_vault(root).unwrap().happened());
        assert!(!legacy.exists());
        assert!(!tmp.path().join(".textree").exists());
    }

    #[test]
    fn settings_carried_out_of_the_folder_name_notes_relative_to_it() {
        // They were written by a release that named notes by their full path. The settings folder
        // they land in is already at the current format, so no format step runs for them.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        prepare_sidecar(root.clone()).unwrap();
        let legacy = legacy_sidecar_path(tmp.path(), "favorites.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let full = tmp.path().join("a.md").to_string_lossy().to_string();
        std::fs::write(&legacy, serde_json::to_vec(&[full]).unwrap()).unwrap();

        assert!(move_state_out_of_vault(root.clone()).unwrap().happened());

        let carried =
            std::fs::read(personal_dir(tmp.path()).unwrap().join("favorites.json")).unwrap();
        let names: Vec<String> = serde_json::from_slice(&carried).unwrap();
        assert_eq!(names, vec!["a.md".to_string()]);
    }

    #[test]
    fn a_settings_file_that_cannot_be_read_is_set_aside_and_kept() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        prepare_sidecar(root.clone()).unwrap();
        write_sidecar(root.clone(), "favorites.json".into(), "{half".into()).unwrap();

        let name = set_aside_sidecar(root.clone(), "favorites.json".into()).unwrap();

        assert!(name.starts_with("favorites.json.unreadable-"), "{name}");
        let dir = personal_dir(tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(&name)).unwrap(), "{half");
        assert_eq!(
            read_sidecar(root.clone(), "favorites.json".into()).unwrap(),
            None
        );
        assert!(set_aside_sidecar(root, "../elsewhere".into()).is_err());
    }

    #[test]
    fn edits_of_different_notes_kept_at_the_same_moment_are_all_kept() {
        let tmp = TempDir::new().unwrap();
        let dir = stranded_dir(tmp.path()).unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let keepers: Vec<_> = (0..8)
            .map(|i| {
                let (dir, start) = (dir.clone(), start.clone());
                std::thread::spawn(move || {
                    let kept = crate::stranded::Kept {
                        rel: format!("note-{i}.md"),
                        text: format!("typed {i}"),
                        base: String::new(),
                        at: 7,
                    };
                    start.wait();
                    crate::stranded::keep(&dir, &kept, tauri_kit_fs::write_atomic_new).unwrap()
                })
            })
            .collect();
        let ids: std::collections::HashSet<String> =
            keepers.into_iter().map(|k| k.join().unwrap()).collect();

        assert_eq!(ids.len(), 8, "every edit got a name of its own");
        let mut texts: Vec<String> = crate::stranded::list(&dir)
            .into_iter()
            .map(|(_, k)| k.text)
            .collect();
        texts.sort();
        assert_eq!(
            texts,
            (0..8).map(|i| format!("typed {i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn kept_edits_come_back_as_the_notes_they_were_typed_into() {
        let tmp = TempDir::new().unwrap();
        let dir = stranded_dir(tmp.path()).unwrap();
        let kept = crate::stranded::Kept {
            rel: "sub/a.md".into(),
            text: "typed".into(),
            base: "before".into(),
            at: 1,
        };
        let id = crate::stranded::keep(&dir, &kept, tauri_kit_fs::write_atomic_new).unwrap();

        let back = stranded_edits(tmp.path()).unwrap();
        assert_eq!(
            back,
            vec![StrandedEdit {
                id: id.clone(),
                rel: "sub/a.md".into(),
                text: "typed".into(),
                base: "before".into(),
            }]
        );
        assert!(
            !tmp.path().join("stranded").exists(),
            "kept outside the folder, not in it"
        );
        crate::stranded::forget(&dir, &id).unwrap();
        assert!(stranded_edits(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn write_outcome_crosses_the_boundary_as_a_tagged_object() {
        assert_eq!(
            serde_json::to_value(WriteOutcome::Written).unwrap(),
            serde_json::json!({ "kind": "written" })
        );
        assert_eq!(
            serde_json::to_value(WriteOutcome::Conflict { disk: None }).unwrap(),
            serde_json::json!({ "kind": "conflict", "disk": null })
        );
    }

    fn seed_note(root: &Path, rel: &str, body: &str) -> String {
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&target, body).unwrap();
        target.to_string_lossy().to_string()
    }

    #[test]
    fn adding_a_version_of_a_note_records_the_images_it_shows() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        std::fs::create_dir_all(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join("assets/Pasted-1.png"), b"png").unwrap();
        std::fs::write(tmp.path().join("assets/unused.png"), b"png").unwrap();
        std::fs::write(tmp.path().join(".env"), b"KEY=1").unwrap();
        let note = seed_note(
            tmp.path(),
            "a.md",
            "![](assets/Pasted-1.png) [x](.env) [web](https://example.com/a.png)",
        );
        commit_notes(root.clone(), vec![note], "v".into()).unwrap();
        let prepared = crate::git_engine::open_existing(tmp.path())
            .unwrap()
            .unwrap();
        let mut recorded =
            crate::git_engine::tip_paths(prepared.repo(), crate::git_engine::NOTES_REF).unwrap();
        recorded.sort();
        assert_eq!(recorded, vec!["a.md", "assets/Pasted-1.png"]);
    }

    #[test]
    fn a_missing_linked_file_does_not_stop_the_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let note = seed_note(tmp.path(), "a.md", "![](gone.png)");
        assert!(commit_notes(
            tmp.path().to_string_lossy().to_string(),
            vec![note],
            "v".into()
        )
        .unwrap()
        .is_some());
    }

    fn recorded_paths(root: &Path) -> Vec<String> {
        let prepared = crate::git_engine::open_existing(root).unwrap().unwrap();
        let mut recorded =
            crate::git_engine::tip_paths(prepared.repo(), crate::git_engine::NOTES_REF).unwrap();
        recorded.sort();
        recorded
    }

    fn add_version(root: &Path, notes: Vec<String>) -> Result<Option<String>, String> {
        commit_notes(root.to_string_lossy().to_string(), notes, "v".into())
    }

    #[test]
    fn an_ignored_linked_file_is_left_out_and_the_version_goes_through() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "secret.png\n").unwrap();
        std::fs::write(tmp.path().join("secret.png"), b"x").unwrap();
        let note = seed_note(tmp.path(), "a.md", "![](secret.png)");
        assert!(add_version(tmp.path(), vec![note]).unwrap().is_some());
        assert_eq!(recorded_paths(tmp.path()), vec!["a.md"]);
    }

    #[test]
    fn a_file_linked_by_two_notes_is_recorded_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("p.png"), b"x").unwrap();
        let a = seed_note(tmp.path(), "a.md", "![](p.png)");
        let b = seed_note(tmp.path(), "b.md", "![](p.png)");
        add_version(tmp.path(), vec![a, b]).unwrap();
        assert_eq!(recorded_paths(tmp.path()), vec!["a.md", "b.md", "p.png"]);
    }

    #[test]
    fn a_linked_folder_is_left_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let note = seed_note(tmp.path(), "a.md", "[x](sub/)");
        assert!(add_version(tmp.path(), vec![note]).unwrap().is_some());
        assert_eq!(recorded_paths(tmp.path()), vec!["a.md"]);
    }

    #[test]
    fn a_path_that_is_not_a_note_brings_no_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.png"), b"x").unwrap();
        let txt = seed_note(tmp.path(), "notes.txt", "![](a.png)");
        add_version(tmp.path(), vec![txt]).unwrap();
        assert_eq!(recorded_paths(tmp.path()), vec!["notes.txt"]);
    }

    #[test]
    fn links_differing_only_in_case_record_one_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join("assets/x.png"), b"x").unwrap();
        let note = seed_note(tmp.path(), "a.md", "![](assets/x.png) ![](Assets/x.png)");
        add_version(tmp.path(), vec![note]).unwrap();
        assert_eq!(recorded_paths(tmp.path()), vec!["a.md", "assets/x.png"]);
    }

    #[test]
    fn a_linked_file_that_cannot_be_read_does_not_stop_the_version() {
        // A file opened without sharing is refused to readers on Windows (os error 32).
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let tmp = tempfile::TempDir::new().unwrap();
            std::fs::write(tmp.path().join("locked.png"), b"x").unwrap();
            let _held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(tmp.path().join("locked.png"))
                .unwrap();
            let note = seed_note(tmp.path(), "a.md", "![](locked.png)");
            assert!(add_version(tmp.path(), vec![note]).unwrap().is_some());
            assert_eq!(recorded_paths(tmp.path()), vec!["a.md"]);
        }
    }

    #[test]
    fn a_link_through_a_junction_to_outside_the_folder_is_left_out() {
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("o.png"), b"x").unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let link = tmp.path().join("out");
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.path(), &link).is_ok();
        assert!(made, "could not create the junction/symlink for this test");
        let note = seed_note(tmp.path(), "a.md", "![](out/o.png)");
        assert!(add_version(tmp.path(), vec![note]).unwrap().is_some());
        assert_eq!(recorded_paths(tmp.path()), vec!["a.md"]);
    }

    /// Records `rel` under `root` and returns its absolute path.
    fn record_note(root: &Path, rel: &str, body: &str) -> String {
        let path = seed_note(root, rel, body);
        commit_notes(
            root.to_string_lossy().to_string(),
            vec![path.clone()],
            format!("recorded {rel}"),
        )
        .unwrap();
        path
    }

    #[test]
    fn a_notes_recorded_states_come_back_in_order_without_touching_the_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();

        let a = record_note(root, "sub/a.md", "first");
        std::fs::write(&a, "second").unwrap();
        commit_notes(
            root.to_string_lossy().to_string(),
            vec![a.clone()],
            "second".into(),
        )
        .unwrap();
        // A different note's revision must not show up in this note's list.
        record_note(root, "b.md", "unrelated");

        let versions = note_versions(root.to_string_lossy().to_string(), a.clone()).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].message, "second");

        let earlier = note_version_text(
            root.to_string_lossy().to_string(),
            a.clone(),
            versions[1].id.clone(),
        )
        .unwrap();
        assert_eq!(earlier, "first");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "second",
            "reading an earlier state must leave the file as it is"
        );
    }

    /// A bare repository behind `git http-backend`, reachable only with the test server's
    /// secret. `None` when git is not installed.
    fn remote_for_test(tmp: &Path) -> Option<String> {
        git2::Repository::init_bare(tmp.join("repo.git"))
            .unwrap()
            .config()
            .unwrap()
            .set_bool("http.receivepack", true)
            .unwrap();
        crate::git_transport::test_server::serve(tmp.to_path_buf())
    }

    #[test]
    fn a_remote_that_refuses_the_secret_is_not_remembered() {
        let tmp = TempDir::new().unwrap();
        let Some(url) = remote_for_test(tmp.path()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let root = tmp.path().join("notes");
        std::fs::create_dir_all(&root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let err =
            connect_remote(root_s.clone(), url, "someone".into(), "wrong".into()).unwrap_err();
        assert!(err.contains("refused these credentials"), "{err}");
        assert_eq!(remote_connection(root_s.clone()).unwrap(), None);
        assert!(crate::remote::secret(&folder_key(&root)).is_none());
    }

    #[test]
    fn plain_http_to_another_machine_is_refused_before_anything_is_sent() {
        let tmp = TempDir::new().unwrap();
        let root_s = tmp.path().to_string_lossy().to_string();
        let err = connect_remote(
            root_s.clone(),
            "http://example.com/notes.git".into(),
            "".into(),
            "t".into(),
        )
        .unwrap_err();
        assert!(err.contains("https"), "{err}");
        assert!(!tmp.path().join(".git").exists());
    }

    /// The same exchange against a real hosted repository, over https. Run by hand:
    /// `TEXTREE_CHECK_REPO=<https address of an empty private repository>`
    /// `TEXTREE_CHECK_TOKEN=<token with write access>`
    /// `cargo test --lib a_hosted_repository -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real hosted repository and a token"]
    fn a_hosted_repository_carries_notes_between_two_folders() {
        let (Ok(url), Ok(token)) = (
            std::env::var("TEXTREE_CHECK_REPO"),
            std::env::var("TEXTREE_CHECK_TOKEN"),
        ) else {
            panic!("set TEXTREE_CHECK_REPO and TEXTREE_CHECK_TOKEN");
        };
        let tmp = TempDir::new().unwrap();
        let locks = NoteLocks::default();
        let here = tmp.path().join("here");
        let there = tmp.path().join("there");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&there).unwrap();
        let (here_s, there_s) = (
            here.to_string_lossy().to_string(),
            there.to_string_lossy().to_string(),
        );

        let stamp = format!("# Check\n\n{}\n", std::process::id());
        seed_note(&here, "check.md", &stamp);
        commit_notes(
            here_s.clone(),
            vec![here.join("check.md").to_string_lossy().to_string()],
            "check".into(),
        )
        .unwrap();
        connect_remote(here_s.clone(), url.clone(), "".into(), token.clone()).unwrap();
        let sent = sync_remote(here_s.clone(), &locks).unwrap();
        assert!(sent.sent, "{sent:?}");
        assert!(notes_backed_up(here_s.clone()).unwrap());

        connect_remote(there_s.clone(), url, "".into(), token).unwrap();
        let got = sync_remote(there_s.clone(), &locks).unwrap();
        assert!(got.received.contains(&"check.md".to_string()), "{got:?}");
        assert_eq!(
            std::fs::read_to_string(there.join("check.md")).unwrap(),
            stamp
        );

        disconnect_remote(here_s).unwrap();
        disconnect_remote(there_s).unwrap();
    }

    #[test]
    fn two_folders_connected_to_one_remote_exchange_what_they_recorded() {
        use crate::git_transport::test_server::{SECRET, USER};
        let tmp = TempDir::new().unwrap();
        let Some(url) = remote_for_test(tmp.path()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let locks = NoteLocks::default();
        let here = tmp.path().join("here");
        let there = tmp.path().join("there");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&there).unwrap();
        let (here_s, there_s) = (
            here.to_string_lossy().to_string(),
            there.to_string_lossy().to_string(),
        );

        seed_note(&here, "manual.md", "# Manual\n");
        commit_notes(
            here_s.clone(),
            vec![here.join("manual.md").to_string_lossy().to_string()],
            "first".into(),
        )
        .unwrap();
        connect_remote(here_s.clone(), url.clone(), USER.into(), SECRET.into()).unwrap();
        assert_eq!(
            remote_connection(here_s.clone()).unwrap(),
            Some(crate::remote::Connection {
                url: url.clone(),
                username: USER.into()
            })
        );
        let sent = sync_remote(here_s.clone(), &locks).unwrap();
        assert!(sent.sent, "{sent:?}");

        assert!(
            notes_backed_up(here_s.clone()).unwrap(),
            "sent, so kept on the remote"
        );
        seed_note(&here, "faq.md", "# FAQ\n");
        commit_notes(
            here_s.clone(),
            vec![here.join("faq.md").to_string_lossy().to_string()],
            "second".into(),
        )
        .unwrap();
        assert!(
            !notes_backed_up(here_s.clone()).unwrap(),
            "recorded since, only here"
        );
        assert!(sync_remote(here_s.clone(), &locks).unwrap().sent);
        assert!(notes_backed_up(here_s.clone()).unwrap());

        // A new machine: an empty folder connected to the remote receives everything.
        assert!(!notes_backed_up(there_s.clone()).unwrap(), "not connected");
        connect_remote(there_s.clone(), url, USER.into(), SECRET.into()).unwrap();
        let got = sync_remote(there_s.clone(), &locks).unwrap();
        assert_eq!(
            got.received,
            vec!["faq.md".to_string(), "manual.md".to_string()]
        );
        assert!(
            notes_backed_up(there_s.clone()).unwrap(),
            "what arrived is on the remote already"
        );
        assert_eq!(
            std::fs::read_to_string(there.join("manual.md")).unwrap(),
            "# Manual\n"
        );

        disconnect_remote(here_s.clone()).unwrap();
        disconnect_remote(there_s.clone()).unwrap();
        assert_eq!(remote_connection(here_s).unwrap(), None);
        assert!(crate::remote::secret(&folder_key(&there)).is_none());
    }

    #[test]
    fn a_choice_is_recorded_without_touching_the_folder() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        seed_note(tmp.path(), "a.md", "# A\n");
        let mut before: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let after_set = set_publish_choice(root.clone(), "a.md".into(), Some(true)).unwrap();
        assert_eq!(
            after_set,
            vec![crate::publish_scope::Choice {
                path: "a.md".into(),
                include: true
            }]
        );
        let mut after: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        after.retain(|n| n != ".git");
        before.sort();
        after.sort();
        assert_eq!(after, before);
        assert!(set_publish_choice(root, "../x.md".into(), Some(true)).is_err());
    }

    #[test]
    fn a_choice_is_recorded_under_plain_names_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        seed_note(tmp.path(), "sub/a.md", "# A");
        // Paths arrive as the tree names them; another spelling of one is refused, as everywhere.
        for odd in ["sub/./a.md", "sub/gone/", "sub\\a.md"] {
            assert!(
                set_publish_choice(root.clone(), odd.into(), Some(true)).is_err(),
                "{odd}"
            );
        }
        let got = set_publish_choice(root.clone(), "sub/a.md".into(), Some(true)).unwrap();
        assert_eq!(
            got,
            vec![crate::publish_scope::Choice {
                path: "sub/a.md".into(),
                include: true
            }]
        );
        // A note gone by now can still be chosen for.
        let got = set_publish_choice(root, "sub/gone".into(), Some(false)).unwrap();
        assert!(
            got.contains(&crate::publish_scope::Choice {
                path: "sub/gone".into(),
                include: false
            }),
            "{got:?}"
        );
    }

    #[test]
    fn gate_l2_8_a_folder_renamed_onto_a_deleted_ones_place_keeps_its_own_choice() {
        // Both alphabetical orders: the moved folder sorting before the deleted one, and after.
        for (deleted, moved) in [("public", "drafts"), ("alpha", "zeta")] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().to_string_lossy().to_string();
            record_note(tmp.path(), &format!("{deleted}/post.md"), "# Post");
            record_note(tmp.path(), &format!("{moved}/diary.md"), "# Diary");
            set_publish_choice(root.clone(), deleted.into(), Some(true)).unwrap();
            set_publish_choice(root.clone(), moved.into(), Some(false)).unwrap();
            delete_node(
                root.clone(),
                tmp.path().join(deleted).to_string_lossy().to_string(),
            )
            .unwrap();
            rename_node(
                root.clone(),
                tmp.path().join(moved).to_string_lossy().to_string(),
                deleted.into(),
            )
            .unwrap();
            assert_eq!(
                publish_choices(root.clone()).unwrap(),
                vec![crate::publish_scope::Choice {
                    path: deleted.into(),
                    include: false
                }],
                "{moved} -> {deleted}"
            );
            let preview = publish_preview(root).unwrap();
            assert!(
                preview.notes.is_empty(),
                "{moved} -> {deleted}: {preview:?}"
            );
        }
    }

    #[test]
    fn gate_l2_8_a_note_made_where_a_deleted_one_was_follows_its_folder() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let note = record_note(tmp.path(), "a.md", "# A");
        set_publish_choice(root.clone(), "a.md".into(), Some(true)).unwrap();
        delete_node(root.clone(), note).unwrap();
        assert!(
            publish_choices(root.clone()).unwrap().is_empty(),
            "the choice went with the note"
        );
        create_note(root.clone(), root.clone(), "a".into()).unwrap();
        record_note(tmp.path(), "a.md", "# Another A");
        assert!(publish_choices(root.clone()).unwrap().is_empty());
        assert!(publish_preview(root).unwrap().nothing_chosen);
    }

    #[test]
    fn gate_l2_8_a_folder_renamed_to_the_name_of_a_note_inside_keeps_that_note_left_out() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        record_note(
            tmp.path(),
            "drafts/blog.md",
            "# Not a folder note of drafts",
        );
        set_publish_choice(root.clone(), "".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "drafts/blog.md".into(), Some(false)).unwrap();
        rename_node(
            root.clone(),
            tmp.path().join("drafts").to_string_lossy().to_string(),
            "blog".into(),
        )
        .unwrap();
        let mut got = publish_choices(root.clone()).unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            vec![
                crate::publish_scope::Choice {
                    path: "".into(),
                    include: true
                },
                crate::publish_scope::Choice {
                    path: "blog/blog.md".into(),
                    include: false
                },
            ]
        );
        let p = publish_preview(root).unwrap();
        assert!(!p.notes.contains(&"blog/blog.md".to_string()), "{p:?}");
        assert!(
            !p.never_recorded.contains(&"blog/blog.md".to_string()),
            "{p:?}"
        );
    }

    #[test]
    fn gate_l2_8_a_folder_renamed_with_its_own_note_carries_that_notes_leave_out() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        record_note(tmp.path(), "drafts/drafts.md", "# Drafts");
        record_note(tmp.path(), "drafts/a.md", "# A");
        set_publish_choice(root.clone(), "".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "drafts/drafts.md".into(), Some(false)).unwrap();
        rename_node(
            root.clone(),
            tmp.path().join("drafts").to_string_lossy().to_string(),
            "blog".into(),
        )
        .unwrap();
        assert!(tmp.path().join("blog/blog.md").is_file());
        let got = publish_choices(root.clone()).unwrap();
        assert!(
            got.contains(&crate::publish_scope::Choice {
                path: "blog/blog.md".into(),
                include: false
            }),
            "{got:?}"
        );
        assert_eq!(publish_preview(root).unwrap().notes, vec!["blog/a.md"]);
    }

    #[test]
    fn gate_l2_8_a_left_out_note_deleted_and_brought_back_is_still_left_out() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        record_note(tmp.path(), "blog/post.md", "# Post");
        let draft = record_note(tmp.path(), "blog/draft.md", "# Draft");
        set_publish_choice(root.clone(), "blog".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "blog/draft.md".into(), Some(false)).unwrap();
        delete_node(root.clone(), draft).unwrap();
        let back = restore_deleted(root.clone(), "blog/draft.md".into()).unwrap();
        assert_eq!(back.rel, "blog/draft.md");
        assert!(tmp.path().join("blog/draft.md").is_file());
        assert_eq!(publish_preview(root).unwrap().notes, vec!["blog/post.md"]);
    }

    #[test]
    fn gate_l2_8_a_note_made_where_a_left_out_one_was_deleted_stays_left_out() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let note = record_note(tmp.path(), "diary.md", "# Diary");
        set_publish_choice(root.clone(), "".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "diary.md".into(), Some(false)).unwrap();
        delete_node(root.clone(), note).unwrap();
        create_note(root.clone(), root.clone(), "diary".into()).unwrap();
        record_note(tmp.path(), "diary.md", "# Another diary");
        let p = publish_preview(root).unwrap();
        assert!(p.notes.is_empty(), "{p:?}");
    }

    #[test]
    fn something_made_drops_a_choice_left_where_it_now_is() {
        use base64::Engine;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let note = record_note(tmp.path(), "a.md", "# A");
        // Left behind by things removed outside the app.
        for rel in ["b.md", "box", "assets", "gone", "gone/n.md"] {
            set_publish_choice(root.clone(), rel.into(), Some(true)).unwrap();
        }
        create_note_with_content(root.clone(), root.clone(), "b".into(), "# B".into()).unwrap();
        create_folder(root.clone(), root.clone(), "box".into()).unwrap();
        let png = base64::engine::general_purpose::STANDARD.encode(b"png");
        save_attachment(root.clone(), note, png, "png".into()).unwrap();
        record_note(tmp.path(), "gone/n.md", "# N");
        std::fs::remove_dir_all(tmp.path().join("gone")).unwrap();
        restore_deleted(root.clone(), "gone/n.md".into()).unwrap();
        let left = publish_choices(root).unwrap();
        assert!(
            left.is_empty(),
            "each came into existence with no choice: {left:?}"
        );
    }

    #[test]
    fn gate_l2_8_a_folder_made_by_promoting_a_note_has_no_choice_of_its_own() {
        use crate::publish_scope::Choice;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let foo = record_note(tmp.path(), "foo.md", "# Foo");
        let bar = record_note(tmp.path(), "bar.md", "# Bar");
        let src = record_note(tmp.path(), "src.md", "# Src");
        // Left behind by folders of the same names removed outside the app.
        set_publish_choice(root.clone(), "foo".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "bar".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "foo.md".into(), Some(false)).unwrap();
        set_publish_choice(root.clone(), "src.md".into(), Some(false)).unwrap();

        promote_node(root.clone(), foo).unwrap();
        adopt_node(root.clone(), src, bar).unwrap();
        let mut got = publish_choices(root.clone()).unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            vec![
                Choice {
                    path: "bar/src.md".into(),
                    include: false
                },
                Choice {
                    path: "foo/foo.md".into(),
                    include: false
                },
            ]
        );
        assert!(!crate::publish_scope::is_included("foo/Untitled.md", &got));
        assert!(!crate::publish_scope::is_included("bar/bar.md", &got));
    }

    #[test]
    fn gate_l2_8_making_a_note_in_a_folder_with_no_history_starts_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        create_note(root.clone(), root.clone(), "a".into()).unwrap();
        create_folder(root.clone(), root, "box".into()).unwrap();
        assert!(!tmp.path().join(".git").exists());
    }

    #[test]
    fn a_note_named_from_its_title_keeps_its_choice() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let note = record_note(tmp.path(), "Untitled.md", "# Trip");
        set_publish_choice(root.clone(), "Untitled.md".into(), Some(true)).unwrap();
        rename_note_unique(root.clone(), note, "Trip".into()).unwrap();
        assert_eq!(
            publish_choices(root.clone()).unwrap(),
            vec![crate::publish_scope::Choice {
                path: "Trip.md".into(),
                include: true
            }]
        );
        assert_eq!(publish_preview(root).unwrap().notes, vec!["Trip.md"]);
    }

    #[test]
    fn the_preview_lists_what_goes_out_under_the_choices() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        record_note(tmp.path(), "a.md", "# A");
        record_note(tmp.path(), "b.md", "# B");
        std::fs::write(tmp.path().join("a.md"), "# A edited").unwrap();
        std::fs::write(tmp.path().join("c.md"), "# C").unwrap();
        assert!(publish_preview(root.clone()).unwrap().nothing_chosen);
        set_publish_choice(root.clone(), "".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "b.md".into(), Some(false)).unwrap();
        let p = publish_preview(root).unwrap();
        assert!(!p.nothing_chosen);
        assert_eq!(p.notes, vec!["a.md"]);
        assert_eq!(p.unrecorded, vec!["a.md"]);
        assert_eq!(p.never_recorded, vec!["c.md"]);
    }

    #[test]
    fn a_linked_file_over_the_size_limit_is_not_recorded_and_is_named_too_large() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        std::fs::File::create(tmp.path().join("big.bin"))
            .unwrap()
            .set_len(crate::publish_assembly::MAX_LINKED_FILE_BYTES + 1)
            .unwrap();
        seed_note(tmp.path(), "small.png", "png");
        record_note(tmp.path(), "a.md", "[b](big.bin) ![s](small.png)");
        let prepared = crate::git_engine::prepare(tmp.path()).unwrap();
        let recorded =
            crate::git_engine::tip_paths(prepared.repo(), crate::git_engine::NOTES_REF).unwrap();
        assert!(
            recorded.iter().any(|p| p == "a.md") && recorded.iter().any(|p| p == "small.png"),
            "{recorded:?}"
        );
        assert!(!recorded.iter().any(|p| p == "big.bin"), "{recorded:?}");

        set_publish_choice(root.clone(), "".into(), Some(true)).unwrap();
        let p = publish_preview(root).unwrap();
        assert_eq!(p.notes, vec!["a.md"]);
        assert_eq!(p.files, vec!["small.png"]);
        assert_eq!(p.too_large, vec!["big.bin"]);
        assert!(
            p.missing.is_empty() && p.unpublished_links.is_empty(),
            "{p:?}"
        );
    }

    #[test]
    fn publishing_records_the_published_state_only_once_it_went_out() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        record_note(tmp.path(), "a.md", "# A");
        set_publish_choice(root, "".into(), Some(true)).unwrap();
        let published = |p: &crate::git_engine::VaultRepo| {
            p.repo()
                .refname_to_id(crate::publish_assembly::PUBLISHED_REF)
                .ok()
        };

        let staged = stage(tmp.path()).unwrap();
        let failed: Result<(), String> =
            publish_staged(&staged, |_| Err("the host refused".into()));
        assert!(failed.is_err());
        assert_eq!(
            published(&staged.prepared),
            None,
            "nothing went out, so nothing is recorded"
        );

        let mut rendered_from = None;
        publish_staged(&staged, |source| {
            rendered_from = Some(std::fs::read_to_string(source.join("a.md")).unwrap());
            Ok(())
        })
        .unwrap();
        assert_eq!(rendered_from.as_deref(), Some("# A"));
        let tip = published(&staged.prepared).expect("recorded after it went out");
        let tree = staged
            .prepared
            .repo()
            .find_commit(tip)
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_name("a.md").is_some());
    }

    #[test]
    fn previewing_a_folder_with_no_history_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_note(tmp.path(), "a.md", "# A");
        let p = publish_preview(tmp.path().to_string_lossy().to_string()).unwrap();
        assert!(p.nothing_chosen);
        assert!(!tmp.path().join(".git").exists());
    }

    #[test]
    fn the_preview_names_linked_files_held_back_by_a_choice() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        seed_note(tmp.path(), "pics/p.png", "bytes");
        record_note(tmp.path(), "a.md", "![p](pics/p.png)");
        set_publish_choice(root.clone(), "".into(), Some(true)).unwrap();
        set_publish_choice(root.clone(), "pics".into(), Some(false)).unwrap();
        let p = publish_preview(root).unwrap();
        assert_eq!(p.notes, vec!["a.md"]);
        assert!(p.files.is_empty());
        assert_eq!(p.withheld, vec!["pics/p.png"]);
    }

    #[test]
    fn the_preview_reads_history_from_an_enclosing_repository() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let root = tmp.path().join("notes");
        std::fs::create_dir(&root).unwrap();
        record_note(&root, "kept.md", "as recorded");
        seed_note(&root, "fresh.md", "never recorded");
        let root_s = root.to_string_lossy().to_string();
        set_publish_choice(root_s.clone(), "".into(), Some(true)).unwrap();

        let preview = publish_preview(root_s).unwrap();
        assert_eq!(preview.notes, vec!["kept.md"]);
        assert_eq!(preview.never_recorded, vec!["fresh.md"]);
        assert!(preview.unrecorded.is_empty());
    }

    #[test]
    fn publish_preview_refuses_a_path_that_is_not_a_folder() {
        let tmp = TempDir::new().unwrap();
        let file = seed_note(tmp.path(), "a.md", "x");
        assert!(publish_preview(file).is_err());
    }

    #[test]
    fn deleted_notes_gathers_both_the_recorded_and_the_never_recorded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let recorded = record_note(root, "kept.md", "recorded body");
        let never = seed_note(root, "sub/scratch.md", "never recorded");
        record_note(root, "staying.md", "still here");

        delete_node(root_s.clone(), recorded).unwrap();
        delete_node(root_s.clone(), never).unwrap();

        let deleted = deleted_notes(root_s.clone()).unwrap();
        let names: Vec<&str> = deleted.iter().map(|d| d.rel.as_str()).collect();
        assert!(
            names.contains(&"kept.md"),
            "a recorded note that was deleted"
        );
        assert!(
            names.contains(&"sub/scratch.md"),
            "and one that was only ever kept when it went"
        );
        assert!(
            !names.contains(&"staying.md"),
            "a note still in the folder is not deleted"
        );
        assert!(
            deleted
                .iter()
                .find(|d| d.rel == "kept.md")
                .unwrap()
                .recorded
        );
        assert!(
            !deleted
                .iter()
                .find(|d| d.rel == "sub/scratch.md")
                .unwrap()
                .recorded
        );

        // Trash holds the moved files, so nothing above depended on them still being in place.
        assert!(!root.join("kept.md").exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_note_another_program_looks_at_for_a_moment_is_still_deleted_and_kept() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        let note = seed_note(root, "sub/held.md", "held a moment");
        // Open the way a scanner does: others may read, not delete.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&note)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });

        delete_node(root_s.clone(), note.clone()).unwrap();
        release.join().unwrap();

        assert!(!Path::new(&note).exists());
        let deleted = deleted_notes(root_s).unwrap();
        assert_eq!(deleted.len(), 1, "and it can be brought back");
    }

    #[cfg(windows)]
    #[test]
    fn negative_control_a_note_held_the_whole_time_is_not_deleted() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        let note = seed_note(root, "sub/held.md", "held");
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&note)
            .unwrap();

        assert!(
            delete_node(root_s, note.clone()).is_err(),
            "the system refuses while it is held"
        );
        assert!(Path::new(&note).exists());
    }

    #[test]
    fn a_note_brought_back_never_lands_on_a_file_that_arrived_first() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let there = root.join("a.md");
        std::fs::write(&there, "arrived first").unwrap();

        let refused = atomic_create_bytes(root, &there, b"brought back").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&there).unwrap(), "arrived first");
        atomic_create_bytes(root, &root.join("b.md"), b"new").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("b.md")).unwrap(), "new");
    }

    #[test]
    fn a_note_is_not_deleted_when_there_is_nowhere_to_keep_it() {
        // A `.git` file pointing at a repository that is gone: the folder is governed by nothing
        // that can be opened, and nothing can be made there either.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".git"), "gitdir: ./moved-away\n").unwrap();
        let root_s = root.to_string_lossy().to_string();
        let note = seed_note(root, "only-copy.md", "nowhere else");
        assert!(
            crate::git_engine::prepare(root).is_err(),
            "precondition: no repository"
        );

        assert!(delete_node(root_s, note.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "nowhere else");
    }

    #[test]
    fn restoring_brings_a_note_back_and_takes_it_off_the_list() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let never = seed_note(root, "sub/scratch.md", "never recorded");
        delete_node(root_s.clone(), never).unwrap();

        let landed = restore_deleted(root_s.clone(), "sub/scratch.md".into()).unwrap();
        assert_eq!(landed.rel, "sub/scratch.md");
        assert_eq!(
            std::fs::read_to_string(root.join("sub").join("scratch.md")).unwrap(),
            "never recorded"
        );
        assert!(
            deleted_notes(root_s.clone()).unwrap().is_empty(),
            "what is back in the folder is no longer missing from it"
        );
    }

    #[test]
    fn looking_at_history_in_a_plain_folder_leaves_it_a_plain_folder() {
        // Opening a panel is not recording. A folder nothing has been recorded in answers
        // "nothing" and stays exactly as it was found.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let root_s = root.to_string_lossy().to_string();
        let note = seed_note(root, "a.md", "body");

        assert!(deleted_notes(root_s.clone()).unwrap().is_empty());
        assert!(note_versions(root_s.clone(), note.clone())
            .unwrap()
            .is_empty());
        assert!(note_version_text(root_s.clone(), note, "0".repeat(40)).is_err());
        assert!(restore_deleted(root_s, "a.md".into()).is_err());

        assert!(
            !root.join(".git").exists(),
            "a folder nothing was recorded in is not turned into a repository by reading"
        );
    }

    #[test]
    fn a_deleted_notes_history_can_still_be_read() {
        // Every entry in the deleted list is a name the folder no longer has, so requiring the
        // path to exist would make previewing before restoring impossible.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "sub/a.md", "first");
        delete_node(root_s.clone(), note.clone()).unwrap();
        assert!(!root.join("sub").join("a.md").exists());

        let versions = note_versions(root_s.clone(), note.clone()).unwrap();
        assert_eq!(versions.len(), 1, "the note is gone; its history is not");
        assert_eq!(
            note_version_text(root_s.clone(), note, versions[0].id.clone()).unwrap(),
            "first"
        );

        // The check still refuses what it is there to refuse.
        let outside = TempDir::new().unwrap();
        assert!(note_versions(
            root_s,
            outside
                .path()
                .join("elsewhere.md")
                .to_string_lossy()
                .to_string()
        )
        .is_err());
    }

    /// Puts a folder into the shape an earlier version of the application left behind.
    fn as_an_earlier_version_left_it(root: &Path, deleted: &[(&str, &str, u64)]) {
        let trash = root.join(".textree").join("trash");
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::write(root.join(".textree").join("favorites.json"), "[\"a.md\"]").unwrap();
        let items: Vec<TrashItem> = deleted
            .iter()
            .map(|(name, body, when)| {
                std::fs::write(trash.join(name), body).unwrap();
                TrashItem {
                    trash_name: (*name).to_string(),
                    original_rel: (*name).to_string(),
                    deleted_at: *when,
                    is_dir: false,
                }
            })
            .collect();
        std::fs::write(
            root.join(".textree").join(TRASH_MANIFEST),
            serde_json::to_string(&items).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn moving_state_out_keeps_every_file_and_leaves_the_folder_clean() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(
            root,
            &[
                ("one.md", "first", 1_700_000_000),
                ("two.md", "second", 1_700_000_500),
            ],
        );

        let moved = move_state_out_of_vault(root_s.clone()).unwrap();

        assert_eq!(moved.notes, 2, "every set-aside file is accounted for");
        assert!(moved.settings);
        assert!(
            !root.join(".textree").exists(),
            "the notes folder holds only notes afterwards"
        );

        // The count is the point: nothing was thrown away, it was carried.
        let deleted = deleted_notes(root_s.clone()).unwrap();
        assert_eq!(deleted.len(), 2);
        assert_eq!(
            read_sidecar(root_s.clone(), "favorites.json".into()).unwrap(),
            Some("[\"a.md\"]".to_string()),
            "settings are readable from where they live now"
        );

        // And the contents came with them.
        restore_deleted(root_s, "one.md".into()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("one.md")).unwrap(),
            "first"
        );
    }

    #[test]
    fn moving_state_out_keeps_when_each_note_left() {
        // The set-aside copies are the only record of that, for anything deleted before the
        // history started keeping it. Dating them all "now" would silently reorder the list.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(
            root,
            &[
                ("older.md", "a", 1_600_000_000),
                ("newer.md", "b", 1_700_000_000),
            ],
        );

        move_state_out_of_vault(root_s.clone()).unwrap();

        let listed = deleted_notes(root_s).unwrap();
        assert_eq!(
            listed.iter().map(|d| d.rel.as_str()).collect::<Vec<_>>(),
            vec!["newer.md", "older.md"]
        );
        assert_eq!(listed[1].seconds, 1_600_000_000);
    }

    #[test]
    fn moving_state_out_a_second_time_reports_that_nothing_happened() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[("one.md", "first", 1_700_000_000)]);

        assert!(move_state_out_of_vault(root_s.clone()).unwrap().happened());
        let again = move_state_out_of_vault(root_s.clone()).unwrap();
        assert_eq!(again, MoveOut::default(), "there is nothing left to move");
        assert_eq!(
            deleted_notes(root_s).unwrap().len(),
            1,
            "and nothing was doubled"
        );
    }

    /// How many states the deleted-content history holds.
    fn snapshots_recorded(root: &Path) -> usize {
        let repo = git2::Repository::open(root).unwrap();
        let Ok(reference) = repo.find_reference(crate::git_engine::SNAPSHOT_REF) else {
            return 0;
        };
        let mut walk = repo.revwalk().unwrap();
        walk.push(reference.target().unwrap()).unwrap();
        walk.count()
    }

    #[test]
    fn a_copy_the_list_never_named_is_carried_too() {
        // The earlier version kept a copy whose entry it could not write, and offered it back as
        // being of unknown origin, to the top of the folder. Leaving it behind would make
        // something that version could restore unreachable here.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[("listed.md", "a", 1_700_000_000)]);
        std::fs::write(
            root.join(".textree").join("trash").join("stray.md"),
            "only copy",
        )
        .unwrap();

        let moved = move_state_out_of_vault(root_s.clone()).unwrap();

        assert_eq!(moved.notes, 2);
        assert!(
            moved.left_behind.is_empty(),
            "left: {:?}",
            moved.left_behind
        );
        assert!(
            !root.join(".textree").exists(),
            "the folder holds only notes afterwards"
        );
        let listed: Vec<String> = deleted_notes(root_s.clone())
            .unwrap()
            .into_iter()
            .map(|d| d.rel)
            .collect();
        assert!(
            listed.contains(&"stray.md".to_string()),
            "listed: {listed:?}"
        );
        restore_deleted(root_s, "stray.md".into()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("stray.md")).unwrap(),
            "only copy"
        );
    }

    #[test]
    fn a_damaged_list_does_not_hide_the_copies_it_described() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(
            root,
            &[
                ("one.md", "first", 1_700_000_000),
                ("two.md", "second", 1_700_000_500),
            ],
        );
        std::fs::write(root.join(".textree").join(TRASH_MANIFEST), "{not json").unwrap();

        let moved = move_state_out_of_vault(root_s.clone()).unwrap();

        assert_eq!(moved.notes, 2);
        assert!(!root.join(".textree").exists());
        assert_eq!(deleted_notes(root_s).unwrap().len(), 2);
    }

    #[test]
    fn a_deleted_folder_counts_its_notes_and_its_other_files_apart() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[]);
        let proj = root.join(".textree").join("trash").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("plan.md"), "# plan").unwrap();
        std::fs::write(proj.join("diagram.png"), [0u8, 1, 2]).unwrap();
        let items = vec![TrashItem {
            trash_name: "proj".into(),
            original_rel: "proj".into(),
            deleted_at: 1_700_000_000,
            is_dir: true,
        }];
        std::fs::write(
            root.join(".textree").join(TRASH_MANIFEST),
            serde_json::to_string(&items).unwrap(),
        )
        .unwrap();

        let moved = move_state_out_of_vault(root_s).unwrap();

        assert_eq!((moved.notes, moved.files), (1, 1));
        assert!(!root.join(".textree").exists());
    }

    #[test]
    fn what_cannot_be_carried_stays_is_reported_and_nothing_is_carried_twice() {
        // A name that cannot exist inside the folder stays where it is. Opening the folder again
        // must neither record the others a second time nor pretend the folder is clean.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[("one.md", "first", 1_700_000_000)]);
        std::fs::write(root.join(".textree").join("trash").join(".odd"), "kept").unwrap();
        std::fs::write(root.join(".textree").join("db.json"), "{}").unwrap();

        let first = move_state_out_of_vault(root_s.clone()).unwrap();
        let recorded = snapshots_recorded(root);
        let second = move_state_out_of_vault(root_s.clone()).unwrap();

        let expected: Vec<String> = [
            ".textree/db.json",
            ".textree/trash.json",
            ".textree/trash/.odd",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(first.notes, 1);
        assert_eq!(first.left_behind, expected);
        assert_eq!(
            (second.notes, second.files, second.settings),
            (0, 0, false),
            "carried once only"
        );
        assert_eq!(
            second.left_behind, expected,
            "still said, every time it is still true"
        );
        assert_eq!(
            snapshots_recorded(root),
            recorded,
            "no state recorded twice"
        );
        assert_eq!(deleted_notes(root_s).unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join(".textree").join("trash").join(".odd")).unwrap(),
            "kept"
        );
    }

    /// Writes the trash list an earlier version kept, exactly as given.
    fn listing(root: &Path, items: &[(&str, &str, u64, bool)]) {
        let items: Vec<TrashItem> = items
            .iter()
            .map(|(name, origin, when, is_dir)| TrashItem {
                trash_name: (*name).into(),
                original_rel: (*origin).into(),
                deleted_at: *when,
                is_dir: *is_dir,
            })
            .collect();
        std::fs::write(
            root.join(".textree").join(TRASH_MANIFEST),
            serde_json::to_string(&items).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_path_deleted_twice_keeps_both_copies_reachable() {
        // The earlier version kept a copy each time and could restore either. Carried under one
        // path, only the latest would be reachable from any screen.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[]);
        let trash = root.join(".textree").join("trash");
        std::fs::write(trash.join("note.md"), "first draft").unwrap();
        std::fs::write(trash.join("note (1).md"), "second draft").unwrap();
        listing(
            root,
            &[
                ("note.md", "note.md", 1_600_000_000, false),
                ("note (1).md", "note.md", 1_700_000_000, false),
            ],
        );

        let moved = move_state_out_of_vault(root_s.clone()).unwrap();

        assert_eq!(moved.notes, 2);
        assert!(!root.join(".textree").exists());
        restore_deleted(root_s.clone(), "note.md".into()).unwrap();
        restore_deleted(root_s, "note (1).md".into()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("note.md")).unwrap(),
            "second draft",
            "the latest keeps the path"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("note (1).md")).unwrap(),
            "first draft",
            "the earlier one is still there"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_copy_arriving_on_a_later_open_does_not_write_over_one_already_carried() {
        // The earlier copy of a path deleted twice cannot be read on the first open, so only the
        // later one is carried. When the earlier one arrives on the next open, it must not take
        // the path the later one already holds.
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[]);
        let trash = root.join(".textree").join("trash");
        std::fs::write(trash.join("note.md"), "first draft").unwrap();
        std::fs::write(trash.join("note (1).md"), "second draft").unwrap();
        listing(
            root,
            &[
                ("note.md", "note.md", 1_600_000_000, false),
                ("note (1).md", "note.md", 1_700_000_000, false),
            ],
        );
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x4)
            .open(trash.join("note.md"))
            .unwrap();

        let first = move_state_out_of_vault(root_s.clone()).unwrap();
        drop(held);
        let second = move_state_out_of_vault(root_s.clone()).unwrap();

        assert_eq!((first.notes, second.notes), (1, 1));
        assert!(!root.join(".textree").exists());
        restore_deleted(root_s.clone(), "note.md".into()).unwrap();
        restore_deleted(root_s, "note (1).md".into()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("note.md")).unwrap(),
            "second draft",
            "the one carried first keeps its path"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("note (1).md")).unwrap(),
            "first draft"
        );
    }

    #[test]
    fn an_empty_deleted_folder_holds_nothing_and_does_not_keep_the_folder() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[]);
        std::fs::create_dir_all(
            root.join(".textree")
                .join("trash")
                .join("empty")
                .join("inner"),
        )
        .unwrap();
        listing(root, &[("empty", "empty", 1_700_000_000, true)]);

        let moved = move_state_out_of_vault(root_s).unwrap();

        assert!(
            moved.left_behind.is_empty(),
            "left: {:?}",
            moved.left_behind
        );
        assert!(
            !root.join(".textree").exists(),
            "nothing was in it, so nothing keeps the folder"
        );
    }

    #[test]
    fn a_list_with_no_copies_beside_it_is_simply_gone() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".textree")).unwrap();
        std::fs::write(root.join(".textree").join(TRASH_MANIFEST), "[]").unwrap();

        let moved = move_state_out_of_vault(root.to_string_lossy().to_string()).unwrap();

        assert_eq!(moved, MoveOut::default());
        assert!(!root.join(".textree").exists());
        assert!(
            !root.join(".git").exists(),
            "and no repository is made for it"
        );
    }

    #[test]
    fn a_leftover_the_report_cannot_name_is_still_reported() {
        // A directory that stays is itself something left behind, even with no file to name.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let inside = root.join(".textree");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(still_inside(root, &inside), vec![".textree".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn a_copy_that_cannot_be_read_stays_whole_and_its_readable_siblings_are_carried_once() {
        // A deleted folder where one file is held open elsewhere (a sync client, a scanner). The
        // readable file is carried and leaves; the unreadable one is not removed with its folder,
        // and opening again records nothing twice.
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();
        as_an_earlier_version_left_it(root, &[]);
        let proj = root.join(".textree").join("trash").join("proj");
        std::fs::create_dir_all(proj.join("deep")).unwrap();
        std::fs::write(proj.join("plan.md"), "# plan").unwrap();
        std::fs::write(proj.join("deep").join("held.md"), "only copy").unwrap();
        listing(root, &[("proj", "proj", 1_700_000_000, true)]);
        let held = std::fs::OpenOptions::new()
            .read(true)
            // FILE_SHARE_DELETE only: nobody else may read it, but it could be deleted — so a
            // wholesale removal of the folder would destroy it, which is what this guards.
            .share_mode(0x4)
            .open(proj.join("deep").join("held.md"))
            .unwrap();

        let first = move_state_out_of_vault(root_s.clone()).unwrap();
        let recorded = snapshots_recorded(root);
        let second = move_state_out_of_vault(root_s.clone()).unwrap();
        drop(held);

        assert_eq!(first.notes, 1);
        assert_eq!(
            first.left_behind,
            vec![
                ".textree/trash.json".to_string(),
                ".textree/trash/proj/deep/held.md".to_string()
            ]
        );
        assert!(!proj.join("plan.md").exists(), "the carried file left");
        assert_eq!(
            std::fs::read_to_string(proj.join("deep").join("held.md")).unwrap(),
            "only copy"
        );
        assert_eq!((second.notes, second.files), (0, 0));
        assert_eq!(snapshots_recorded(root), recorded, "nothing recorded twice");

        // Once it can be read, the next open carries it and the folder is clean.
        let third = move_state_out_of_vault(root_s).unwrap();
        assert_eq!(third.notes, 1);
        assert!(!root.join(".textree").exists());
    }

    #[test]
    fn a_folder_that_never_held_anything_of_ours_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        seed_note(root, "a.md", "body");
        let moved = move_state_out_of_vault(root.to_string_lossy().to_string()).unwrap();
        assert_eq!(moved, MoveOut::default());
        assert!(
            !root.join(".git").exists(),
            "and no repository is made for it"
        );
    }

    #[test]
    fn going_back_to_an_earlier_state_keeps_the_one_it_replaces() {
        // Going back is the one action that overwrites work rather than adding to it. Work that
        // was never recorded has nowhere else to be, so it has to land somewhere reachable
        // before the older state is written over it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "a.md", "first");
        std::fs::write(&note, "worked on since, never recorded").unwrap();

        let first = note_versions(root_s.clone(), note.clone()).unwrap()[0]
            .id
            .clone();
        restore_version(root_s.clone(), note.clone(), first).unwrap();

        assert_eq!(std::fs::read_to_string(&note).unwrap(), "first");

        // The replaced state is reachable: deleting the note now brings it back, because what
        // was kept is newer than anything recorded.
        delete_node(root_s.clone(), note).unwrap();
        let back = restore_deleted(root_s, "a.md".into()).unwrap();
        assert!(back.as_deleted);
        assert_eq!(
            std::fs::read_to_string(root.join("a.md")).unwrap(),
            "first",
            "and the last state on disk was the one that was put back"
        );
    }

    #[test]
    fn going_back_needs_a_state_that_belongs_to_this_note() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let a = record_note(root, "a.md", "mine");
        let b = record_note(root, "b.md", "someone else's");
        let other = note_versions(root_s.clone(), b).unwrap()[0].id.clone();

        assert!(restore_version(root_s, a.clone(), other).is_err());
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "mine",
            "a refused request leaves the note alone"
        );
    }

    #[test]
    fn using_an_alternative_writes_the_note_records_it_and_keeps_what_it_replaced() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "a.md", "first");
        let started = start_alternative(root_s.clone(), note.clone()).unwrap();
        assert_eq!(started.rel, "a.md");
        assert!(!started.differs);
        add_alternative_version(
            root_s.clone(),
            started.id.clone(),
            "other take".into(),
            "try".into(),
        )
        .unwrap();
        let listed = list_alternatives(root_s.clone()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].differs);
        assert_eq!(
            alternative_text(root_s.clone(), started.id.clone()).unwrap(),
            "other take"
        );

        // Edits never recorded are on disk when the alternative is picked.
        std::fs::write(&note, "worked on since, never recorded").unwrap();
        use_alternative(root_s.clone(), note.clone(), started.id.clone()).unwrap();

        assert_eq!(std::fs::read_to_string(&note).unwrap(), "other take");
        assert!(list_alternatives(root_s.clone()).unwrap().is_empty());
        let versions = note_versions(root_s.clone(), note.clone()).unwrap();
        assert_eq!(
            versions[0].message.lines().next(),
            Some("Used an alternative")
        );
        // What it replaced is reachable, as when going back to an earlier version.
        delete_node(root_s.clone(), note).unwrap();
        assert!(restore_deleted(root_s, "a.md".into()).unwrap().as_deleted);
        assert_eq!(
            std::fs::read_to_string(root.join("a.md")).unwrap(),
            "other take",
            "the last state on disk came back"
        );
    }

    #[test]
    fn an_alternative_is_used_only_for_its_own_note_and_setting_it_aside_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let a = record_note(root, "a.md", "mine");
        let b = record_note(root, "b.md", "bee");
        let alt = start_alternative(root_s.clone(), a.clone()).unwrap();
        add_alternative_version(root_s.clone(), alt.id.clone(), "take".into(), "try".into())
            .unwrap();

        assert!(use_alternative(root_s.clone(), b.clone(), alt.id.clone()).is_err());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bee");

        set_aside_alternative(root_s.clone(), alt.id.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "mine");
        assert!(list_alternatives(root_s.clone()).unwrap().is_empty());
        assert!(
            use_alternative(root_s, a, alt.id).is_err(),
            "one that ended cannot be used"
        );
    }

    #[test]
    fn what_is_typed_into_an_alternative_stays_out_of_the_folder_until_it_is_a_version() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "a.md", "mine");
        let alt = start_alternative(root_s.clone(), note.clone()).unwrap();
        assert_eq!(
            alternative_draft(root_s.clone(), alt.id.clone()).unwrap(),
            None
        );

        write_alternative_draft(root_s.clone(), alt.id.clone(), "half a thought".into()).unwrap();
        assert_eq!(
            alternative_draft(root_s.clone(), alt.id.clone())
                .unwrap()
                .as_deref(),
            Some("half a thought")
        );
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "mine",
            "the note is the other side"
        );
        assert_eq!(
            alternative_text(root_s.clone(), alt.id.clone()).unwrap(),
            "mine"
        );
        let in_folder: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            in_folder.len(),
            2,
            "only the note and the repository: {in_folder:?}"
        );

        // Adding exactly what was typed makes it a version and lets the draft go.
        add_alternative_version(
            root_s.clone(),
            alt.id.clone(),
            "half a thought".into(),
            "v".into(),
        )
        .unwrap();
        assert_eq!(
            alternative_text(root_s.clone(), alt.id.clone()).unwrap(),
            "half a thought"
        );
        assert_eq!(
            alternative_draft(root_s.clone(), alt.id.clone()).unwrap(),
            None
        );

        // Typed on after the text was taken: the newer draft is kept.
        write_alternative_draft(root_s.clone(), alt.id.clone(), "a whole thought".into()).unwrap();
        add_alternative_version(
            root_s.clone(),
            alt.id.clone(),
            "half a thought, then".into(),
            "v".into(),
        )
        .unwrap();
        assert_eq!(
            alternative_draft(root_s, alt.id).unwrap().as_deref(),
            Some("a whole thought")
        );
    }

    #[test]
    fn ending_an_alternative_keeps_what_was_still_being_typed_into_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        // Used: the note gets what was last typed, not the last version.
        let a = record_note(root, "a.md", "mine");
        let used = start_alternative(root_s.clone(), a.clone()).unwrap();
        write_alternative_draft(root_s.clone(), used.id.clone(), "typed, never added".into())
            .unwrap();
        use_alternative(root_s.clone(), a.clone(), used.id.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "typed, never added");
        assert!(
            alternative_draft(root_s.clone(), used.id.clone()).is_err(),
            "it has ended"
        );

        // Set aside: the note stays, and the ended alternative holds what was typed.
        let b = record_note(root, "b.md", "bee");
        let aside = start_alternative(root_s.clone(), b.clone()).unwrap();
        write_alternative_draft(root_s.clone(), aside.id.clone(), "an idea for later".into())
            .unwrap();
        set_aside_alternative(root_s.clone(), aside.id.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bee");
        let repo = git2::Repository::open(root).unwrap();
        let ended = crate::alternatives::ended(&repo).unwrap();
        let kept = ended
            .iter()
            .find(|e| e.id == aside.id)
            .expect("kept as ended");
        assert_eq!(
            crate::alternatives::content(&repo, kept)
                .unwrap()
                .as_deref(),
            Some("an idea for later".as_bytes())
        );
        // Nothing is left behind for either.
        let drafts = drafts_dir(root).unwrap();
        assert!(!drafts.join(&used.id).exists() && !drafts.join(&aside.id).exists());
        // And nothing more can be typed into one that ended.
        assert!(write_alternative_draft(root_s, aside.id.clone(), "late".into()).is_err());
        assert!(!drafts.join(&aside.id).exists());
    }

    #[test]
    fn a_folder_with_no_history_has_no_alternatives_and_cannot_start_one() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        seed_note(root, "a.md", "body");
        let root_s = root.to_string_lossy().to_string();
        assert!(list_alternatives(root_s.clone()).unwrap().is_empty());
        assert!(
            start_alternative(root_s, root.join("a.md").to_string_lossy().to_string()).is_err()
        );
        assert!(!root.join(".git").exists(), "looking made no repository");
    }

    #[test]
    fn a_note_can_only_be_brought_back_from_one_place() {
        // There used to be two: the history, and a copy set aside inside the folder. Taking
        // both offers put two of the same note there — the thing a version history exists to
        // stop. Deleting no longer sets anything aside, so there is one offer.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = seed_note(root, "a.md", "body");
        delete_node(root_s.clone(), note).unwrap();

        assert_eq!(deleted_notes(root_s.clone()).unwrap().len(), 1);
        restore_deleted(root_s.clone(), "a.md".into()).unwrap();
        assert!(deleted_notes(root_s).unwrap().is_empty());
        assert!(root.join("a.md").exists());
        assert!(!root.join("a (1).md").exists());
    }

    #[test]
    fn renaming_a_recorded_note_does_not_make_it_look_deleted() {
        // Nothing was deleted, so nothing belongs in the deleted list. Offering the old name
        // back would put a second, older copy of a live note into the folder.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "before.md", "body");
        rename_node(root_s.clone(), note, "after".into()).unwrap();

        assert!(
            deleted_notes(root_s.clone()).unwrap().is_empty(),
            "a note that was renamed is still in the folder"
        );
        assert!(root.join("after.md").exists());
    }

    #[test]
    fn a_renamed_notes_earlier_states_are_still_its_own() {
        // Following the move is the other half of writing it: the states from before the
        // rename were recorded by the same person about the same note.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "before.md", "first");
        std::fs::write(&note, "second").unwrap();
        commit_notes(root_s.clone(), vec![note.clone()], "second".into()).unwrap();
        let after = rename_node(root_s.clone(), note, "after".into()).unwrap();

        let versions = note_versions(root_s.clone(), after.clone()).unwrap();
        assert_eq!(
            versions.len(),
            2,
            "the rename is followed, and is not itself one of the states"
        );
        assert_eq!(versions[0].message, "second");
        assert_eq!(
            note_version_text(root_s, after, versions[1].id.clone()).unwrap(),
            "first",
            "the earliest state is readable under the new name"
        );
    }

    #[test]
    fn moving_a_folder_carries_every_recorded_note_in_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        record_note(root, "src/a.md", "a");
        record_note(root, "src/deep/b.md", "b");
        std::fs::create_dir_all(root.join("dest")).unwrap();

        move_node(
            root_s.clone(),
            root.join("src").to_string_lossy().to_string(),
            root.join("dest").to_string_lossy().to_string(),
        )
        .unwrap();

        assert!(
            deleted_notes(root_s.clone()).unwrap().is_empty(),
            "moving a folder deletes none of the notes in it"
        );
        assert_eq!(
            note_versions(
                root_s,
                root.join("dest")
                    .join("src")
                    .join("deep")
                    .join("b.md")
                    .to_string_lossy()
                    .to_string(),
            )
            .unwrap()
            .len(),
            1,
            "a note one level down keeps its history too"
        );
    }

    #[test]
    fn renaming_a_folder_carries_the_note_named_after_it() {
        // A folder rename is two moves on disk: the folder, and the note that carries its
        // name. Following only the first leaves the second recorded under a name nothing is at.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        record_note(root, "topic/topic.md", "the folder note");
        record_note(root, "topic/child.md", "a child");

        rename_node(
            root_s.clone(),
            root.join("topic").to_string_lossy().to_string(),
            "subject".into(),
        )
        .unwrap();

        assert!(
            deleted_notes(root_s.clone()).unwrap().is_empty(),
            "renaming a folder deletes neither it nor anything in it"
        );
        assert_eq!(
            note_versions(
                root_s,
                root.join("subject")
                    .join("subject.md")
                    .to_string_lossy()
                    .to_string(),
            )
            .unwrap()
            .len(),
            1,
            "the note the folder is named after keeps its history under the new name"
        );
    }

    #[test]
    fn turning_a_note_into_a_folder_carries_its_history_down_with_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "topic.md", "body");
        promote_node(root_s.clone(), note).unwrap();

        assert!(
            deleted_notes(root_s.clone()).unwrap().is_empty(),
            "the note is still there, one level down"
        );
        assert_eq!(
            note_versions(
                root_s,
                root.join("topic")
                    .join("topic.md")
                    .to_string_lossy()
                    .to_string(),
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn adopting_carries_both_the_promoted_note_and_the_one_taken_in() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let host = record_note(root, "host.md", "the host");
        let guest = record_note(root, "guest.md", "the guest");

        adopt_node(root_s.clone(), guest, host).unwrap();

        assert!(
            deleted_notes(root_s.clone()).unwrap().is_empty(),
            "adopting is two moves, and neither of them is a deletion"
        );
        for landed in ["host.md", "guest.md"] {
            assert_eq!(
                note_versions(
                    root_s.clone(),
                    root.join("host").join(landed).to_string_lossy().to_string(),
                )
                .unwrap()
                .len(),
                1,
                "{landed} keeps its history"
            );
        }
    }

    #[test]
    fn renaming_a_note_that_was_never_recorded_starts_no_history() {
        // There is nothing to carry over, and moving a file is not the moment to begin
        // recording one.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let root_s = root.to_string_lossy().to_string();

        let note = seed_note(root, "scratch.md", "body");
        rename_node(root_s, note, "renamed".into()).unwrap();

        assert!(
            !root.join(".git").exists(),
            "a folder nothing was recorded in stays a plain folder"
        );
    }

    #[test]
    fn restoring_brings_back_edits_that_were_never_recorded() {
        // The note was saved once, worked on afterwards without being saved again, then
        // deleted. Only the second state answers "give me back what I deleted"; handing over
        // the recorded one throws away the work and says nothing about having done so.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "a.md", "as recorded");
        std::fs::write(&note, "worked on since, never recorded").unwrap();
        delete_node(root_s.clone(), note).unwrap();

        let landed = restore_deleted(root_s.clone(), "a.md".into()).unwrap();
        assert!(landed.as_deleted, "it came back as it was when it went");
        assert_eq!(
            std::fs::read_to_string(root.join("a.md")).unwrap(),
            "worked on since, never recorded"
        );
    }

    #[test]
    fn restoring_a_note_removed_outside_the_application_uses_what_was_recorded() {
        // Nothing was kept on the way out, because the application was not the one that took
        // it. History is all there is, and the answer says so.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "a.md", "as recorded");
        std::fs::remove_file(&note).unwrap();

        let landed = restore_deleted(root_s.clone(), "a.md".into()).unwrap();
        assert!(!landed.as_deleted, "there was no kept state to prefer");
        assert_eq!(
            std::fs::read_to_string(root.join("a.md")).unwrap(),
            "as recorded"
        );
    }

    #[test]
    fn restoring_prefers_a_version_saved_after_the_note_was_last_kept() {
        // Deleted, brought back, saved, then removed some other way. What was kept is now the
        // older of the two, and preferring it by rule would hand back a stale copy.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = seed_note(root, "a.md", "first pass");
        delete_node(root_s.clone(), note).unwrap();
        restore_deleted(root_s.clone(), "a.md".into()).unwrap();

        // A revision has to land in a later second than the kept one for "newer" to mean
        // anything at all — timestamps here have one-second resolution.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let note = record_note(root, "a.md", "second pass, recorded");
        std::fs::remove_file(&note).unwrap();

        let landed = restore_deleted(root_s.clone(), "a.md".into()).unwrap();
        assert!(!landed.as_deleted);
        assert_eq!(
            std::fs::read_to_string(root.join("a.md")).unwrap(),
            "second pass, recorded"
        );
    }

    #[test]
    fn the_deleted_list_is_ordered_by_when_notes_left_the_folder() {
        // An old note deleted just now belongs above a recent one deleted before it. Ordering
        // by when each was last saved gets that backwards.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let long_ago = record_note(root, "long-ago.md", "written first");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let recent = record_note(root, "recent.md", "written later");

        // The one saved first is the one deleted last.
        delete_node(root_s.clone(), recent).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        delete_node(root_s.clone(), long_ago).unwrap();

        let deleted = deleted_notes(root_s.clone()).unwrap();
        let names: Vec<&str> = deleted.iter().map(|d| d.rel.as_str()).collect();
        assert_eq!(
            names,
            vec!["long-ago.md", "recent.md"],
            "newest first means most recently deleted, not most recently saved"
        );
    }

    #[test]
    fn recording_a_note_that_has_not_changed_reports_that_nothing_was_added() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = record_note(root, "a.md", "unchanged");
        let again = commit_notes(root_s.clone(), vec![note.clone()], "again".into()).unwrap();

        assert!(
            again.is_none(),
            "nothing was written, so there is no revision to point at"
        );
        assert_eq!(
            note_versions(root_s, note).unwrap().len(),
            1,
            "and the history it would have been part of is unchanged"
        );
    }

    #[test]
    fn restoring_over_an_existing_name_keeps_both() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        let note = seed_note(root, "a.md", "the deleted one");
        delete_node(root_s.clone(), note).unwrap();
        seed_note(root, "a.md", "a different note with the same name");

        let landed = restore_deleted(root_s.clone(), "a.md".into()).unwrap();
        assert_eq!(landed.rel, "a (1).md");
        assert_eq!(
            std::fs::read_to_string(root.join("a.md")).unwrap(),
            "a different note with the same name",
            "what was already there must survive"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a (1).md")).unwrap(),
            "the deleted one"
        );
    }

    #[test]
    fn restoring_refuses_a_path_that_climbs_out_of_the_vault() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        let root_s = root.to_string_lossy().to_string();

        for attempt in ["../escape.md", "sub/../../escape.md", ""] {
            assert!(restore_deleted(root_s.clone(), attempt.into()).is_err());
        }
        assert!(
            !root.parent().unwrap().join("escape.md").exists(),
            "nothing may be written outside the vault"
        );
    }

    #[test]
    fn committing_records_the_named_paths_and_nothing_else() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = git2::Repository::init(root).unwrap();

        // Unrelated work the person has staged. It must come through untouched.
        std::fs::write(root.join("app.txt"), "their work").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("app.txt")).unwrap();
        index.write().unwrap();
        let staged_before = index.len();

        let a = seed_note(root, "a.md", "# a");
        let b = seed_note(root, "sub/b.md", "# b");

        let oid = commit_notes(
            root.to_string_lossy().to_string(),
            vec![a, b],
            "record two notes".into(),
        )
        .unwrap();
        assert_eq!(
            oid.expect("something was written").len(),
            40,
            "a revision identifier is returned"
        );

        let tree = repo
            .find_reference(crate::git_engine::NOTES_REF)
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_path(Path::new("a.md")).is_ok());
        assert!(tree.get_path(&Path::new("sub").join("b.md")).is_ok());
        assert!(
            tree.get_path(Path::new("app.txt")).is_err(),
            "staged work must not be swept in"
        );

        let index_after = repo.index().unwrap();
        assert_eq!(index_after.len(), staged_before, "the index is untouched");
    }

    #[test]
    fn committing_is_refused_while_the_repository_is_busy() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = git2::Repository::init(root).unwrap();
        let a = seed_note(root, "a.md", "# a");
        std::fs::write(repo.path().join("MERGE_HEAD"), "0\n").unwrap();

        let err = commit_notes(
            root.to_string_lossy().to_string(),
            vec![a],
            "record a note".into(),
        )
        .unwrap_err();

        assert!(err.contains("another operation"), "got: {err}");
        assert!(
            repo.find_reference(crate::git_engine::NOTES_REF).is_err(),
            "nothing may be recorded when the attempt is refused"
        );
    }

    #[test]
    fn committing_an_ignored_path_is_refused_rather_than_silently_dropped() {
        // Without this, the history simply would not contain what was asked for, and there
        // would be nothing on screen to explain why.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = git2::Repository::init(root).unwrap();
        std::fs::write(root.join(".gitignore"), "drafts/\n").unwrap();
        let hidden = seed_note(root, "drafts/a.md", "# a");

        let err = commit_notes(
            root.to_string_lossy().to_string(),
            vec![hidden],
            "record a note".into(),
        )
        .unwrap_err();

        assert!(err.contains("ignore rule"), "got: {err}");
        assert!(repo.find_reference(crate::git_engine::NOTES_REF).is_err());
    }

    #[test]
    fn committing_a_path_outside_the_vault_is_refused() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("vault");
        std::fs::create_dir_all(&root).unwrap();
        git2::Repository::init(&root).unwrap();
        let outside = seed_note(tmp.path(), "elsewhere.md", "# elsewhere");

        let err = commit_notes(
            root.to_string_lossy().to_string(),
            vec![outside],
            "record a note".into(),
        )
        .unwrap_err();
        assert!(err.contains("outside the vault"), "got: {err}");
    }

    #[test]
    fn committing_a_folder_says_so_instead_of_failing_obscurely() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git2::Repository::init(root).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();

        let err = commit_notes(
            root.to_string_lossy().to_string(),
            vec![root.join("sub").to_string_lossy().to_string()],
            "record".into(),
        )
        .unwrap_err();
        assert!(err.contains("is a folder"), "got: {err}");
    }

    #[test]
    fn committing_nothing_is_refused() {
        let tmp = TempDir::new().unwrap();
        let err = commit_notes(
            tmp.path().to_string_lossy().to_string(),
            vec![],
            "record".into(),
        )
        .unwrap_err();
        assert!(err.contains("nothing was selected"), "got: {err}");
    }

    #[test]
    fn settings_live_outside_the_notes_folder_and_stay_confined() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let settings = sidecar_path(root, "favorites.json").unwrap();
        assert!(
            !settings.starts_with(root),
            "a preference is not a note and does not belong in the notes folder"
        );
        assert!(settings.ends_with("favorites.json"));
        assert!(sidecar_path(root, "views/board.json")
            .unwrap()
            .ends_with("board.json"));

        // Two folders that end with the same name still get their own place.
        let same_name = TempDir::new().unwrap();
        std::fs::create_dir(same_name.path().join("notes")).unwrap();
        std::fs::create_dir(root.join("notes")).unwrap();
        assert_ne!(
            sidecar_path(&root.join("notes"), "favorites.json").unwrap(),
            sidecar_path(&same_name.path().join("notes"), "favorites.json").unwrap()
        );

        assert!(sidecar_path(root, "../secret").is_err());
        assert!(sidecar_path(root, "a/../../b").is_err());
        assert!(sidecar_path(root, "/etc/passwd").is_err());
        assert!(sidecar_path(root, "").is_err());
        assert!(sidecar_path(root, ".").is_err());
    }

    #[test]
    fn sidecar_write_then_read_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        assert_eq!(
            read_sidecar(root.clone(), "favorites.json".into()).unwrap(),
            None
        );
        write_sidecar(root.clone(), "favorites.json".into(), "[\"a.md\"]".into()).unwrap();
        assert_eq!(
            read_sidecar(root.clone(), "favorites.json".into()).unwrap(),
            Some("[\"a.md\"]".to_string())
        );
        write_sidecar(root.clone(), "views/b.json".into(), "{}".into()).unwrap();
        assert_eq!(
            read_sidecar(root.clone(), "views/b.json".into()).unwrap(),
            Some("{}".to_string())
        );
        assert!(write_sidecar(root.clone(), "../x".into(), "{}".into()).is_err());
    }

    #[test]
    fn atomic_write_replaces_existing_content() {
        let root = TempDir::new().unwrap();
        let f = root.path().join("note.md");
        std::fs::write(&f, "old").unwrap();
        atomic_write(root.path(), &f, "new content").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new content");
    }

    #[test]
    fn atomic_write_creates_when_absent() {
        let root = TempDir::new().unwrap();
        let f = root.path().join("fresh.md");
        atomic_write(root.path(), &f, "hi").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hi");
    }

    #[test]
    fn temp_dir_lives_in_repository_storage() {
        let root = TempDir::new().unwrap();
        let repo = git2::Repository::init(root.path()).unwrap();
        assert_eq!(temp_dir(root.path()), Some(repo.path().join(TEMP_DIR_NAME)));
    }

    #[test]
    fn temp_dir_of_a_nested_folder_uses_the_governing_repository() {
        // The folder someone opens may sit inside a repository they are working in. Staging
        // under that folder would put temp files in their working tree; the repository's own
        // storage is outside it.
        let root = TempDir::new().unwrap();
        let repo = git2::Repository::init(root.path()).unwrap();
        let inner = root.path().join("docs");
        std::fs::create_dir_all(&inner).unwrap();

        assert_eq!(temp_dir(&inner), Some(repo.path().join(TEMP_DIR_NAME)));
        assert!(!temp_dir(&inner).unwrap().starts_with(&inner));
    }

    #[test]
    fn a_folder_no_repository_governs_gets_no_staging_directory() {
        // Making one would put something of ours in a folder that holds only notes.
        assert_eq!(temp_dir(Path::new("/nowhere-that-exists")), None);
    }

    #[test]
    fn writing_before_anything_was_recorded_leaves_only_the_note() {
        // A first-run default folder, or any folder opened before its first version: writing
        // must still be atomic, and must still leave the folder holding only notes.
        let root = TempDir::new().unwrap();
        let f = root.path().join("welcome.md");
        atomic_write(root.path(), &f, "first").unwrap();
        atomic_write(root.path(), &f, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&f).unwrap(), "second");
        let names: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["welcome.md".to_string()],
            "no directory, no temp, no repository"
        );
    }

    #[test]
    fn a_temp_a_crash_left_at_the_top_of_the_folder_is_swept_and_nothing_else_is() {
        let root = TempDir::new().unwrap();
        std::fs::write(
            root.path().join(format!("{LOOSE_TEMP_PREFIX}abc123")),
            "half written",
        )
        .unwrap();
        std::fs::write(root.path().join(".tmpNotOurs"), "someone else's").unwrap();
        std::fs::write(root.path().join("note.md"), "note").unwrap();

        clear_temp_dir(root.path());

        let mut names: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![".tmpNotOurs".to_string(), "note.md".to_string()]
        );
    }

    #[test]
    fn atomic_write_leaves_no_temp_beside_the_target() {
        // Temp files must not litter the user's content folders (sync tools churn on them),
        // and must not appear as changes in a repository they are working in.
        let root = TempDir::new().unwrap();
        git2::Repository::init(root.path()).unwrap();
        let notes = root.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        let f = notes.join("foo.md");
        atomic_write(root.path(), &f, "body").unwrap();

        assert_eq!(std::fs::read_to_string(&f).unwrap(), "body");
        let entries: Vec<_> = std::fs::read_dir(&notes).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "only foo.md, no temp litter");
        assert!(temp_dir(root.path()).unwrap().is_dir());
        assert!(
            !root.path().join(".textree").exists(),
            "nothing of ours is left in the folder itself"
        );
    }

    #[test]
    fn clear_temp_dir_also_sweeps_the_older_location() {
        // Upgrading must not leave orphans behind at the previous address.
        let root = TempDir::new().unwrap();
        git2::Repository::init(root.path()).unwrap();
        let legacy = root.path().join(".textree").join("tmp");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(".tmpOrphan"), "stale").unwrap();

        clear_temp_dir(root.path());
        assert_eq!(std::fs::read_dir(&legacy).unwrap().flatten().count(), 0);
    }

    #[test]
    fn clear_temp_dir_removes_orphaned_temps() {
        // A crash mid-write can orphan a temp; it would otherwise sync forever. Cleared on open.
        let root = TempDir::new().unwrap();
        git2::Repository::init(root.path()).unwrap();
        let tmp = temp_dir(root.path()).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".tmpOrphan"), "stale").unwrap();
        clear_temp_dir(root.path());
        assert_eq!(std::fs::read_dir(&tmp).unwrap().flatten().count(), 0);
    }
}

#[cfg(test)]
mod onboarding_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn seeds_welcome_in_fresh_vault() {
        let base = tempdir().unwrap();
        let vault = ensure_vault_at(base.path()).unwrap();
        assert_eq!(vault, base.path().join("Textree"));
        let welcome = vault.join("welcome.md");
        assert!(
            welcome.is_file(),
            "welcome.md should be seeded in a fresh vault"
        );
        let body = std::fs::read_to_string(&welcome).unwrap();
        assert!(body.contains("Welcome to Textree"));
    }

    #[test]
    fn does_not_seed_when_markdown_already_exists() {
        let base = tempdir().unwrap();
        let vault_dir = base.path().join("Textree");
        std::fs::create_dir_all(&vault_dir).unwrap();
        std::fs::write(vault_dir.join("note.md"), "my own note").unwrap();
        let vault = ensure_vault_at(base.path()).unwrap();
        assert!(
            !vault.join("welcome.md").exists(),
            "must not seed into a non-empty vault"
        );
        // existing file untouched (non-destructive)
        assert_eq!(
            std::fs::read_to_string(vault_dir.join("note.md")).unwrap(),
            "my own note"
        );
    }

    #[test]
    fn does_not_overwrite_existing_welcome() {
        let base = tempdir().unwrap();
        let vault_dir = base.path().join("Textree");
        std::fs::create_dir_all(&vault_dir).unwrap();
        std::fs::write(vault_dir.join("welcome.md"), "user edited welcome").unwrap();
        ensure_vault_at(base.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(vault_dir.join("welcome.md")).unwrap(),
            "user edited welcome",
            "an existing welcome.md must never be overwritten"
        );
    }

    #[test]
    fn is_idempotent_on_second_call() {
        let base = tempdir().unwrap();
        ensure_vault_at(base.path()).unwrap();
        // user deletes the seed, then app restarts → must NOT re-seed (welcome was intentional, once)
        std::fs::write(base.path().join("Textree").join("keep.md"), "x").unwrap();
        std::fs::remove_file(base.path().join("Textree").join("welcome.md")).unwrap();
        ensure_vault_at(base.path()).unwrap();
        assert!(!base.path().join("Textree").join("welcome.md").exists());
    }

    /// A base path that cannot have `base/Textree` created under it — a regular file occupies a
    /// path component, so `create_dir_all` fails. Mirrors a OneDrive-redirected/uninitialized
    /// Documents dir whose returned path is invalid (the real-world trigger of os error 3).
    fn uncreatable_base() -> (tempfile::TempDir, PathBuf) {
        let holder = tempdir().unwrap();
        let file = holder.path().join("a_file");
        std::fs::write(&file, "x").unwrap();
        // `file` is a regular file, so `file/sub/Textree` can never be created.
        (holder, file.join("sub"))
    }

    #[test]
    fn no_fallback_flag_when_primary_base_works() {
        let good = tempdir().unwrap();
        let bases = vec![good.path().to_path_buf()];
        let (vault, fell_back) = first_creatable_vault(&bases).unwrap();
        assert!(
            !fell_back,
            "a working primary base must not report a fallback"
        );
        assert_eq!(vault, good.path().join("Textree"));
        assert!(vault.join("welcome.md").is_file());
    }

    #[test]
    fn falls_back_when_primary_base_is_uncreatable() {
        let (_holder, invalid) = uncreatable_base();
        let good = tempdir().unwrap();
        // Primary (invalid) fails create_dir_all → must land on the second candidate and flag it.
        let bases = vec![invalid, good.path().to_path_buf()];
        let (vault, fell_back) = first_creatable_vault(&bases).unwrap();
        assert!(
            fell_back,
            "must report falling back away from the unusable primary base"
        );
        assert_eq!(vault, good.path().join("Textree"));
        assert!(
            vault.join("welcome.md").is_file(),
            "the fallback vault is fully seeded"
        );
    }

    #[test]
    fn errors_when_every_candidate_base_fails() {
        let (_holder, invalid) = uncreatable_base();
        let bases = vec![invalid];
        let err = first_creatable_vault(&bases)
            .expect_err("with no creatable base, resolution must error (never silently no-op)");
        // The os error code is preserved (not stringified away) so telemetry can carry it.
        assert!(
            err.1.is_some(),
            "the failing candidate's raw os error code must be captured"
        );
    }
}

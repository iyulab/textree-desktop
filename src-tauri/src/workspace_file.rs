//! The notes folders the sidebar shows, kept from one launch to the next.
//!
//! One file for the device — `workspace.json` in a state directory of its own beside the folders'
//! settings — lists each notes folder by its id and path. It is the person's intent about this
//! device, not something about the notes, so it is never kept inside a notes folder.
//!
//! Losing it must not lose anything else, and a newer release's file must survive an older one:
//! - a file that cannot be read is moved aside, not lost, and the list starts empty — the person
//!   is told where it went;
//! - a directory in a format this release does not know is read but never written: the list shows
//!   what this release understands of it and changes stay in memory for this launch.
//!
//! The ids are the ones [`crate::workspace::id_for`] gives the path, so a folder keeps its id across
//! launches and the window's table and this file agree.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use tauri_kit_state::{Settings, Step, Versioned};

/// The file in the workspace's state directory.
pub const FILE: &str = "workspace.json";

/// Steps from each format to the next. Format 1 is the first that says what it holds; format 2
/// adds `ai` to an entry — a release that knows only 1 must not write the file, since its entries
/// would drop the field (only the top level of a file keeps what a release does not know).
const STEPS: [Step; 2] = [|_, _| Ok(()), |_, _| Ok(())];
const FORMATS: Versioned<'static> = Versioned::new(&STEPS);

/// One notes folder of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    pub id: String,
    pub path: String,
    /// The name the person gave it in the sidebar, when not the folder's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether the folder is used with AI — indexed for it, searched by meaning, asked about. On
    /// unless the person turned it off.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub ai: bool,
}

fn on() -> bool {
    true
}

fn is_on(ai: &bool) -> bool {
    *ai
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Contents {
    roots: Vec<Root>,
}

/// The workspace as this launch found it.
#[derive(Debug)]
pub struct Workspace {
    settings: Settings<Contents>,
    writable: bool,
}

impl Workspace {
    /// The workspace kept in `dir`, brought to the current format when it is in an older one.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let writable = FORMATS.writable(dir)?;
        if writable {
            std::fs::create_dir_all(dir)?;
            FORMATS.bring_up_to_date(dir, &(), tauri_kit_fs::write_atomic)?;
        }
        let settings = Settings::<Contents>::load(&dir.join(FILE))?;
        Ok(Self { settings, writable })
    }

    /// The notes folders, in the sidebar's order.
    pub fn roots(&self) -> &[Root] {
        &self.settings.value().roots
    }

    /// Whether changes are kept: not when the file is in a format this release does not know.
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Where an unreadable file was moved this launch, when it was.
    pub fn set_aside(&self) -> Option<&Path> {
        self.settings.set_aside()
    }

    /// Adds the notes folder at `path` at the end, unless it is there already (however its path is
    /// spelled). Returns its entry. A folder inside one already listed, or holding one, is refused
    /// with what the sidebar says about it: a notes folder's notes belong to one notes folder.
    pub fn add(&mut self, path: &str) -> io::Result<Root> {
        if let Some(same) = self.placed(path, None)? {
            return Ok(same);
        }
        let root = Root {
            id: crate::workspace::id_for(path),
            path: path.to_string(),
            label: None,
            ai: true,
        };
        self.settings.value_mut().roots.push(root.clone());
        self.save()?;
        Ok(root)
    }

    /// Whether the notes folder `id` may be pointed at `path`, where it now is: not when `path` is
    /// another listed folder, inside one or holding one — the same refusals as [`Self::add`].
    pub fn check_relocation(&self, id: &str, path: &str) -> io::Result<()> {
        match self.placed(path, Some(id))? {
            Some(other) => Err(refused(format!(
                "This folder is already in the sidebar as {}.",
                name_of(&other)
            ))),
            None => Ok(()),
        }
    }

    /// Points the notes folder `id` at `path`, where the folder now is — it was moved or renamed,
    /// or its drive is mounted elsewhere. It keeps its place in the sidebar and the name the person
    /// gave it; its id becomes the new path's. Refused as [`Self::check_relocation`] says.
    pub fn relocate(&mut self, id: &str, path: &str) -> io::Result<Root> {
        self.check_relocation(id, path)?;
        let Some(root) = self
            .settings
            .value_mut()
            .roots
            .iter_mut()
            .find(|r| r.id == id)
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "This notes folder is no longer in the sidebar.",
            ));
        };
        root.id = crate::workspace::id_for(path);
        root.path = path.to_string();
        let root = root.clone();
        self.save()?;
        Ok(root)
    }

    /// The listed folder `path` is, however it is spelled — or an error when it is inside a listed
    /// folder or holds one. The folder `except` is passed over.
    fn placed(&self, path: &str, except: Option<&str>) -> io::Result<Option<Root>> {
        let here = crate::search::vault_identity(Path::new(path));
        for root in self
            .roots()
            .iter()
            .filter(|r| Some(r.id.as_str()) != except)
        {
            let there = crate::search::vault_identity(Path::new(&root.path));
            if there == here {
                return Ok(Some(root.clone()));
            }
            if here.starts_with(&there) {
                return Err(refused(format!(
                    "This folder is already in the sidebar inside {}.",
                    name_of(root)
                )));
            }
            if there.starts_with(&here) {
                return Err(refused(format!("{} is inside this folder.", name_of(root))));
            }
        }
        Ok(None)
    }

    /// Takes the notes folder `id` out of the list. The folder and its notes are not touched.
    /// Returns whether it was listed.
    pub fn remove(&mut self, id: &str) -> io::Result<bool> {
        let roots = &mut self.settings.value_mut().roots;
        let before = roots.len();
        roots.retain(|r| r.id != id);
        if roots.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// Puts the notes folders in the order `ids` names them. Folders it does not name keep their
    /// order after those it does; ids not listed are passed over.
    pub fn reorder(&mut self, ids: &[String]) -> io::Result<()> {
        let roots = &mut self.settings.value_mut().roots;
        let rank = |r: &Root| ids.iter().position(|id| *id == r.id).unwrap_or(usize::MAX);
        roots.sort_by_key(rank); // stable: the folders not named keep their order
        self.save()
    }

    /// Names the notes folder `id` in the sidebar; an empty name goes back to the folder's own.
    pub fn relabel(&mut self, id: &str, label: &str) -> io::Result<bool> {
        let label = label.trim();
        let Some(root) = self
            .settings
            .value_mut()
            .roots
            .iter_mut()
            .find(|r| r.id == id)
        else {
            return Ok(false);
        };
        root.label = (!label.is_empty()).then(|| label.to_string());
        self.save()?;
        Ok(true)
    }

    /// Turns the use of a notes folder with AI on or off. Returns whether the folder is listed.
    pub fn set_ai(&mut self, id: &str, ai: bool) -> io::Result<bool> {
        let Some(root) = self
            .settings
            .value_mut()
            .roots
            .iter_mut()
            .find(|r| r.id == id)
        else {
            return Ok(false);
        };
        root.ai = ai;
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> io::Result<()> {
        if self.writable {
            self.settings.save()
        } else {
            Ok(()) // a newer release's file: this launch's changes stay in memory
        }
    }
}

/// What the sidebar calls a notes folder: the name the person gave it, else the folder's own.
pub fn name_of(root: &Root) -> String {
    root.label.clone().unwrap_or_else(|| {
        Path::new(&root.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.path.clone())
    })
}

fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Where the workspace is kept under the base the folders' settings are kept under.
pub fn dir_under(base: &Path) -> PathBuf {
    base.join(".textree").join("workspace")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_new_workspace_is_empty_and_writable_and_says_its_format() {
        let d = dir();
        let ws = Workspace::load(&d.path().join("workspace")).unwrap();
        assert!(ws.roots().is_empty());
        assert!(ws.writable());
        assert!(ws.set_aside().is_none());
        assert_eq!(
            std::fs::read_to_string(
                d.path()
                    .join("workspace")
                    .join(tauri_kit_state::FORMAT_FILE)
            )
            .unwrap()
            .trim(),
            "2"
        );
    }

    #[test]
    fn a_folder_kept_from_ai_stays_so_and_the_others_say_nothing_about_it() {
        let d = dir();
        let mut ws = Workspace::load(d.path()).unwrap();
        let a = ws.add("/notes/a").unwrap();
        ws.add("/notes/b").unwrap();
        assert!(ws.set_ai(&a.id, false).unwrap());
        assert!(!ws.set_ai("unknown", false).unwrap());
        let again = Workspace::load(d.path()).unwrap();
        assert!(!again.roots()[0].ai);
        assert!(again.roots()[1].ai);
        let text = std::fs::read_to_string(d.path().join(FILE)).unwrap();
        assert_eq!(
            text.matches("\"ai\"").count(),
            1,
            "only the folder that differs says so: {text}"
        );
    }

    #[test]
    fn a_list_from_before_ai_was_kept_reads_every_folder_as_used_with_ai() {
        let d = dir();
        std::fs::write(d.path().join(tauri_kit_state::FORMAT_FILE), "1").unwrap();
        std::fs::write(
            d.path().join(FILE),
            r#"{"roots":[{"id":"x","path":"/notes/a"}]}"#,
        )
        .unwrap();
        let ws = Workspace::load(d.path()).unwrap();
        assert!(ws.writable());
        assert!(ws.roots()[0].ai);
    }

    #[test]
    fn folders_are_kept_across_launches_in_order_once_each() {
        let d = dir();
        let mut ws = Workspace::load(d.path()).unwrap();
        let a = ws.add("/notes/a").unwrap();
        ws.add("/notes/b").unwrap();
        assert_eq!(ws.add("/notes/a").unwrap(), a);
        let again = Workspace::load(d.path()).unwrap();
        let paths: Vec<_> = again.roots().iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/notes/a", "/notes/b"]);
        assert_eq!(again.roots()[0].id, crate::workspace::id_for("/notes/a"));
    }

    #[test]
    fn an_unreadable_file_is_moved_aside_not_lost() {
        let d = dir();
        Workspace::load(d.path()).unwrap();
        std::fs::write(d.path().join(FILE), b"{ not json").unwrap();
        let ws = Workspace::load(d.path()).unwrap();
        assert!(ws.roots().is_empty());
        let aside = ws.set_aside().expect("moved aside");
        assert_eq!(std::fs::read(aside).unwrap(), b"{ not json");
    }

    #[test]
    fn a_newer_releases_file_is_read_and_never_written() {
        let d = dir();
        std::fs::write(d.path().join(tauri_kit_state::FORMAT_FILE), "9").unwrap();
        let newer = r#"{"roots":[{"id":"rx","path":"/notes/a","colour":"teal"}],"groups":[1]}"#;
        std::fs::write(d.path().join(FILE), newer).unwrap();
        let mut ws = Workspace::load(d.path()).unwrap();
        assert!(!ws.writable());
        assert_eq!(ws.roots()[0].path, "/notes/a");
        ws.add("/notes/b").unwrap();
        assert_eq!(ws.roots().len(), 2, "kept for this launch");
        assert_eq!(std::fs::read_to_string(d.path().join(FILE)).unwrap(), newer);
        assert_eq!(
            std::fs::read_to_string(d.path().join(tauri_kit_state::FORMAT_FILE)).unwrap(),
            "9"
        );
    }

    #[test]
    fn fields_a_newer_release_added_survive_a_save() {
        let d = dir();
        Workspace::load(d.path()).unwrap();
        std::fs::write(d.path().join(FILE), r#"{"roots":[],"groups":["work"]}"#).unwrap();
        let mut ws = Workspace::load(d.path()).unwrap();
        ws.add("/notes/a").unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(saved["groups"], serde_json::json!(["work"]));
    }
    #[test]
    fn the_same_folder_spelled_another_way_is_listed_once() {
        let d = dir();
        let notes = d.path().join("notes");
        std::fs::create_dir_all(notes.join("sub")).unwrap();
        let mut ws = Workspace::load(&d.path().join("ws")).unwrap();
        let a = ws.add(&notes.to_string_lossy()).unwrap();
        let spelled = notes.join("sub").join("..");
        assert_eq!(ws.add(&spelled.to_string_lossy()).unwrap(), a);
        assert_eq!(ws.roots().len(), 1);
    }

    #[test]
    fn a_folder_inside_a_listed_one_or_holding_one_is_refused_and_a_sibling_is_not() {
        let d = dir();
        let parent = d.path().join("repo");
        for sub in ["docs/inner", "site"] {
            std::fs::create_dir_all(parent.join(sub)).unwrap();
        }
        let mut ws = Workspace::load(&d.path().join("ws")).unwrap();
        ws.add(&parent.join("docs").to_string_lossy()).unwrap();
        let inside = ws
            .add(&parent.join("docs/inner").to_string_lossy())
            .unwrap_err();
        assert_eq!(inside.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            inside.to_string(),
            "This folder is already in the sidebar inside docs."
        );
        let holding = ws.add(&parent.to_string_lossy()).unwrap_err();
        assert_eq!(holding.to_string(), "docs is inside this folder.");
        ws.add(&parent.join("site").to_string_lossy()).unwrap();
        assert_eq!(
            ws.roots().len(),
            2,
            "a sibling of the same repository is its own folder"
        );
    }

    #[test]
    fn a_name_like_another_folders_start_is_not_inside_it() {
        let d = dir();
        for sub in ["notes", "notes-old"] {
            std::fs::create_dir_all(d.path().join(sub)).unwrap();
        }
        let mut ws = Workspace::load(&d.path().join("ws")).unwrap();
        ws.add(&d.path().join("notes").to_string_lossy()).unwrap();
        ws.add(&d.path().join("notes-old").to_string_lossy())
            .unwrap();
        assert_eq!(ws.roots().len(), 2);
    }

    #[test]
    fn removing_reordering_and_naming_are_kept_and_leave_the_folders_alone() {
        let d = dir();
        let mut ws = Workspace::load(d.path()).unwrap();
        let a = ws.add("/notes/a").unwrap();
        let b = ws.add("/notes/b").unwrap();
        let c = ws.add("/notes/c").unwrap();
        ws.reorder(&[c.id.clone(), a.id.clone()]).unwrap();
        assert!(ws.relabel(&a.id, "  Work  ").unwrap());
        assert!(ws.remove(&b.id).unwrap());
        assert!(!ws.remove(&b.id).unwrap(), "already gone");
        let again = Workspace::load(d.path()).unwrap();
        let listed: Vec<_> = again
            .roots()
            .iter()
            .map(|r| (r.path.as_str(), name_of(r)))
            .collect();
        assert_eq!(
            listed,
            [
                ("/notes/c", "c".to_string()),
                ("/notes/a", "Work".to_string())
            ]
        );
        let mut again = again;
        again.relabel(&a.id, " ").unwrap();
        assert_eq!(
            name_of(&again.roots()[1]),
            "a",
            "an empty name goes back to the folder's own"
        );
    }

    #[test]
    fn folders_the_order_does_not_name_keep_their_order_after_it() {
        let d = dir();
        let mut ws = Workspace::load(d.path()).unwrap();
        let ids: Vec<_> = ["/n/a", "/n/b", "/n/c", "/n/d"]
            .iter()
            .map(|p| ws.add(p).unwrap().id)
            .collect();
        ws.reorder(&[ids[2].clone(), "rgone".to_string()]).unwrap();
        let paths: Vec<_> = ws.roots().iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/n/c", "/n/a", "/n/b", "/n/d"]);
    }

    #[test]
    fn a_moved_folder_keeps_its_place_and_name_under_its_new_path() {
        let d = dir();
        let base = tempfile::TempDir::new().unwrap();
        let (a, b, moved) = (
            base.path().join("a"),
            base.path().join("b"),
            base.path().join("b-moved"),
        );
        for p in [&a, &moved] {
            std::fs::create_dir(p).unwrap();
        }
        let mut ws = Workspace::load(d.path()).unwrap();
        let first = ws.add(a.to_str().unwrap()).unwrap();
        let gone = ws.add(b.to_str().unwrap()).unwrap();
        ws.relabel(&gone.id, "Work").unwrap();

        let now = ws.relocate(&gone.id, moved.to_str().unwrap()).unwrap();

        assert_eq!(now.id, crate::workspace::id_for(moved.to_str().unwrap()));
        assert_eq!(now.label.as_deref(), Some("Work"));
        let again = Workspace::load(d.path()).unwrap();
        let ids: Vec<_> = again.roots().iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec![first.id, now.id]);
        assert_eq!(again.roots()[1].path, moved.to_str().unwrap());
    }

    #[test]
    fn a_folder_is_not_pointed_at_another_listed_one_or_inside_one() {
        let d = dir();
        let base = tempfile::TempDir::new().unwrap();
        let (a, b) = (base.path().join("a"), base.path().join("b"));
        std::fs::create_dir_all(a.join("inner")).unwrap();
        let mut ws = Workspace::load(d.path()).unwrap();
        ws.add(a.to_str().unwrap()).unwrap();
        let gone = ws.add(b.to_str().unwrap()).unwrap();

        let same = ws.relocate(&gone.id, a.to_str().unwrap()).unwrap_err();
        assert_eq!(
            same.to_string(),
            "This folder is already in the sidebar as a."
        );
        let inside = ws
            .relocate(&gone.id, a.join("inner").to_str().unwrap())
            .unwrap_err();
        assert_eq!(
            inside.to_string(),
            "This folder is already in the sidebar inside a."
        );
        assert_eq!(ws.roots()[1].path, b.to_str().unwrap(), "nothing changed");
        // Its own path, however spelled, is no conflict with itself.
        assert!(ws.check_relocation(&gone.id, b.to_str().unwrap()).is_ok());
    }
}

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

/// Steps from each format to the next. Format 1 is the first that says what it holds.
const STEPS: [Step; 1] = [|_, _| Ok(())];
const FORMATS: Versioned<'static> = Versioned::new(&STEPS);

/// One notes folder of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    pub id: String,
    pub path: String,
    /// The name the person gave it in the sidebar, when not the folder's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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

    /// Adds the notes folder at `path` at the end, unless it is there already. Returns its entry.
    pub fn add(&mut self, path: &str) -> io::Result<Root> {
        let id = crate::workspace::id_for(path);
        if let Some(found) = self.roots().iter().find(|r| r.id == id) {
            return Ok(found.clone());
        }
        let root = Root {
            id,
            path: path.to_string(),
            label: None,
        };
        self.settings.value_mut().roots.push(root.clone());
        self.save()?;
        Ok(root)
    }

    fn save(&self) -> io::Result<()> {
        if self.writable {
            self.settings.save()
        } else {
            Ok(()) // a newer release's file: this launch's changes stay in memory
        }
    }
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
            "1"
        );
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
}

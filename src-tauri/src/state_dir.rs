//! The format of the settings the application keeps for each notes folder, outside it.
//!
//! That settings folder carries one number: the format its files are in. Each release knows the
//! formats up to its own, and brings a folder in an older one up to date when it is opened, one step
//! at a time. A folder in a format it does not know — written by a newer release, or a marker it
//! cannot read — is left exactly as it is: the application shows its defaults there and writes
//! nothing until it is updated. Otherwise going back to an older release would rewrite the settings
//! in the shape it knows, over the shape it does not, and the newer release would find them mixed or
//! gone.

use std::io;
use std::path::Path;
use tauri_kit_state::{Step, Versioned};

pub use tauri_kit_state::set_aside;
#[cfg(test)]
pub use tauri_kit_state::FORMAT_FILE;

/// The format this release keeps settings in.
pub const CURRENT: u32 = 2;

/// Steps from each format to the next, in order: step `i` brings format `i` to `i + 1`. The format
/// is written only after the last one, so a step cut short runs again on the next open — every step
/// must be safe to run twice, and must write what it makes before removing what it replaces.
/// Each step is handed the notes folder the settings are kept for.
const STEPS: [Step<str>; CURRENT as usize] = [
    // 0 → 1: the files do not change. Format 1 is where the folder starts saying what it holds.
    |_, _| Ok(()),
    // 1 → 2: notes and folders are named relative to the notes folder.
    |dir, root| relative_names_in(dir, root, &tauri_kit_fs::write_atomic),
];

/// The settings files that name notes and folders.
const NAMING_FILES: [&str; 3] = ["favorites.json", "order.json", "views.json"];

const FORMATS: Versioned<'static, str> = Versioned::new(&STEPS);

/// Whether settings may be written into `dir` — not when they are in a format this release does
/// not know.
pub fn writable(dir: &Path) -> io::Result<bool> {
    FORMATS.writable(dir)
}

/// Brings the settings in `dir`, kept for the notes folder at `root`, to the current format.
/// `false`: they are in one this release does not know, and were left untouched. `write` writes a
/// file atomically.
pub fn bring_up_to_date(
    dir: &Path,
    root: &str,
    write: impl Fn(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<bool> {
    FORMATS.bring_up_to_date(dir, root, write)
}

/// Names the notes and folders in every settings file in `dir` relative to `root`. The step from
/// format 1, and what carries settings over from inside an old notes folder — those arrive in the
/// old shape whatever format the folder they land in is at. Names already relative stay as they
/// are, so running it twice changes nothing.
pub fn relative_names_in(
    dir: &Path,
    root: &str,
    write: &impl Fn(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    for name in NAMING_FILES {
        relative_names(&dir.join(name), root, write)?;
    }
    Ok(())
}

/// Rewrites one settings file so the notes and folders it names under `root` are named relative to
/// it (`/`-separated, `""` for the folder itself). A name elsewhere — the folder was moved, or the
/// settings came from another machine — is kept as it is: it is not this release's to discard. A
/// file that cannot be read is left for the screen to set aside.
fn relative_names(
    file: &Path,
    root: &str,
    write: &impl Fn(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Ok(()),
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(());
    };
    let rel = |s: &str| relative_to(root, s).unwrap_or_else(|| s.to_string());
    let changed = match &mut value {
        // favorites: the notes and folders starred.
        serde_json::Value::Array(items) => {
            let mut changed = false;
            for item in items.iter_mut() {
                if let serde_json::Value::String(s) = item {
                    let to = rel(s);
                    changed |= to != *s;
                    *s = to;
                }
            }
            changed
        }
        // order (folder → its notes in order) and views (folder → its views, each naming the
        // folder again).
        serde_json::Value::Object(map) => {
            let mut changed = false;
            let entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            for (key, mut entry) in entries {
                let to = rel(&key);
                changed |= to != key;
                if let serde_json::Value::Array(list) = &mut entry {
                    for item in list.iter_mut() {
                        match item {
                            serde_json::Value::String(s) => {
                                let r = rel(s);
                                changed |= r != *s;
                                *s = r;
                            }
                            serde_json::Value::Object(view) => {
                                if let Some(serde_json::Value::String(s)) = view.get_mut("folder") {
                                    let r = rel(s);
                                    changed |= r != *s;
                                    *s = r;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                map.insert(to, entry);
            }
            changed
        }
        _ => false,
    };
    if changed {
        let bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
        write(file, &bytes)?;
    }
    Ok(())
}

/// `path` relative to `root`, when it is `root` or under it — compared as Windows compares paths:
/// either separator, any case, a trailing separator or not.
fn relative_to(root: &str, path: &str) -> Option<String> {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_string();
    let (root, path) = (norm(root), norm(path));
    if root.is_empty() {
        return None;
    }
    if path.eq_ignore_ascii_case(&root) {
        return Some(String::new());
    }
    let head = path.get(..root.len())?;
    let rest = path.get(root.len()..)?;
    (head.eq_ignore_ascii_case(&root) && rest.starts_with('/')).then(|| rest[1..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn plain_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, bytes)
    }

    #[test]
    fn a_folder_that_never_said_is_brought_to_the_current_format() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("settings");
        assert!(bring_up_to_date(&dir, "C:/notes", plain_write).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join(FORMAT_FILE)).unwrap(),
            CURRENT.to_string()
        );
    }

    fn json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn notes_and_folders_named_under_the_folder_come_to_be_named_relative_to_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join(FORMAT_FILE), "1").unwrap();
        std::fs::write(
            dir.join("favorites.json"),
            r#"["C:\\Notes\\a.md","c:/notes/Projects","D:\\elsewhere\\x.md","already.md"]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("order.json"),
            r#"{"C:\\Notes":["C:\\Notes\\b.md","C:\\Notes\\a.md"],"C:\\Notes\\Projects":["C:\\Notes\\Projects\\p.md"]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("views.json"),
            r#"{"C:\\Notes\\Tasks":[{"version":1,"name":"Open","folder":"C:\\Notes\\Tasks","columns":null,"sort":null,"filters":[]}],"C:/some-old-location/tasks":[]}"#,
        )
        .unwrap();

        assert!(bring_up_to_date(dir, r"C:\Notes\", plain_write).unwrap());
        assert_eq!(
            json(&dir.join("favorites.json")),
            serde_json::json!(["a.md", "Projects", "D:\\elsewhere\\x.md", "already.md"])
        );
        assert_eq!(
            json(&dir.join("order.json")),
            serde_json::json!({"": ["b.md", "a.md"], "Projects": ["Projects/p.md"]})
        );
        let views = json(&dir.join("views.json"));
        assert_eq!(views["Tasks"][0]["folder"], "Tasks");
        // A folder elsewhere is kept as it was, for the screen to say so.
        assert!(views.get("C:/some-old-location/tasks").is_some());
        assert_eq!(std::fs::read_to_string(dir.join(FORMAT_FILE)).unwrap(), "2");

        // Once more changes nothing: a step cut short runs again.
        let before = std::fs::read(dir.join("order.json")).unwrap();
        assert!(bring_up_to_date(dir, r"C:\Notes", plain_write).unwrap());
        assert_eq!(std::fs::read(dir.join("order.json")).unwrap(), before);
    }

    #[test]
    fn settings_that_cannot_be_read_or_a_newer_format_are_left_as_they_are() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("favorites.json"), "not json [").unwrap();
        assert!(bring_up_to_date(tmp.path(), "C:/notes", plain_write).unwrap());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("favorites.json")).unwrap(),
            "not json ["
        );

        let newer = TempDir::new().unwrap();
        std::fs::write(newer.path().join(FORMAT_FILE), (CURRENT + 1).to_string()).unwrap();
        std::fs::write(newer.path().join("favorites.json"), r#"["C:/notes/a.md"]"#).unwrap();
        assert!(!bring_up_to_date(newer.path(), "C:/notes", plain_write).unwrap());
        assert_eq!(
            std::fs::read_to_string(newer.path().join("favorites.json")).unwrap(),
            r#"["C:/notes/a.md"]"#
        );
    }

    #[test]
    fn a_name_that_only_begins_like_the_folder_is_not_under_it() {
        assert_eq!(relative_to("C:/notes", "C:/notes2/a.md"), None);
        assert_eq!(
            relative_to("C:/notes", "C:/NOTES/a.md").as_deref(),
            Some("a.md")
        );
        assert_eq!(
            // public-text: allow — a name in a script without case is what this line is about
            relative_to("C:/노트", "C:/노트/a.md").as_deref(),
            Some("a.md")
        );
    }

    #[test]
    fn a_newer_format_is_not_writable() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(FORMAT_FILE), (CURRENT + 1).to_string()).unwrap();
        assert!(!bring_up_to_date(tmp.path(), "C:/notes", plain_write).unwrap());
        assert!(!writable(tmp.path()).unwrap());
    }
}

//! The notes folders the window knows, named by an id that crosses the IPC instead of a path.
//!
//! A command names the folder it works in by the id [`register`] handed out when the folder was
//! opened, and the command's wrapper turns it back into the path with [`path_of`] before anything
//! else — everything behind the wrapper keeps working on the path, exactly as the tree hands it
//! out, so the per-note turns and the folder's settings are keyed as they always were.
//!
//! An id stays good for the life of the process: an answer that arrives after another folder was
//! opened still names the folder it was asked about.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What a refusal says when a command names a folder that was never opened.
pub const UNKNOWN: &str = "unknown notes folder";

fn table() -> &'static Mutex<HashMap<String, String>> {
    static TABLE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

/// The id for the folder at `path`: the same path always gives the same id. Pure.
pub fn id_for(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("r{hex}")
}

/// Makes the folder at `path` known and returns its id. Opening it again gives the same id.
pub fn register(path: &str) -> String {
    let id = id_for(path);
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), path.to_string());
    id
}

/// The path of the folder `id` names, as it was registered.
pub fn path_of(id: &str) -> Result<String, String> {
    table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
        .ok_or_else(|| UNKNOWN.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_path_gets_the_same_id_and_a_different_one_a_different_id() {
        assert_eq!(id_for("/notes/a"), id_for("/notes/a"));
        assert_ne!(id_for("/notes/a"), id_for("/notes/b"));
        assert!(id_for("/notes/a").starts_with('r'));
    }

    #[test]
    fn a_registered_folder_is_found_by_its_id_exactly_as_registered() {
        let path = "/notes/registered-folder";
        let id = register(path);
        assert_eq!(path_of(&id).unwrap(), path);
        // Opening it again changes nothing.
        assert_eq!(register(path), id);
    }

    #[test]
    fn an_unknown_id_is_refused_and_a_path_is_not_an_id() {
        assert_eq!(path_of("rffffffffffffffff").unwrap_err(), UNKNOWN);
        // The old way of naming a folder — by its path — no longer reaches anything.
        let path = "/notes/never-registered";
        assert_eq!(path_of(path).unwrap_err(), UNKNOWN);
    }
}

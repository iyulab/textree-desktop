//! The app's dialogs, through one route a build made for end-to-end runs can answer
//! ([`tauri_kit_dialog`], the `e2e` feature).

use tauri::AppHandle;
use tauri_kit_dialog::{answer_or, Answer, Ask, Kind};
use tauri_plugin_dialog::DialogExt;

/// The folder the person picks in a dialog titled `title`, or `None` when they close it without
/// one. Blocks until then, so it is never called on the main thread.
pub fn folder(app: &AppHandle, title: &str) -> Result<Option<String>, String> {
    let ask = Ask {
        kind: Kind::Folder,
        title,
        default_path: None,
        filters: &[],
    };
    let answer = answer_or(&ask, || {
        let mut dialog = app.dialog().file();
        if !title.is_empty() {
            dialog = dialog.set_title(title);
        }
        match dialog
            .blocking_pick_folder()
            .map(|picked| picked.into_path())
        {
            Some(Ok(path)) => Answer::Paths(vec![path]),
            // A pick that is not a path on this machine is not a folder that can be opened.
            Some(Err(_)) | None => Answer::Cancelled,
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(match answer {
        Answer::Paths(paths) => paths
            .into_iter()
            .next()
            .map(|p| p.to_string_lossy().into_owned()),
        Answer::Cancelled => None,
    })
}

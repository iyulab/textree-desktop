//! Exchanging recorded notes with a remote the person connected: where a folder's connection is
//! kept (with its repository), connecting, backing-up state, and one exchange.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use tauri_kit_state::folder_key;

use crate::commands::{atomic_write_beside, history_repo, personal_dir, NEWER_SETTINGS};
use crate::note_locks::NoteLocks;

/// Where the remote connection of the folder at `root` is kept, and the key its secret is kept
/// under: those of the repository the folder records into, not of the folder.
///
/// A repository has one set of recorded references and one record of what the remote holds, so
/// it has one remote. Folders of the same repository therefore share it — connecting one connects
/// the others, and they all say the same about being backed up. A folder that is its own
/// repository, or has none yet (recording makes one there), keeps it as before.
///
/// A connection kept with the folder by an earlier version, when the folder sits inside a larger
/// repository, is taken over for the repository the first time it is looked for — unless the
/// repository already has one, which then stands and the folder's is left as it was.
fn remote_place(root: &Path) -> Result<(PathBuf, String), String> {
    let home = match history_repo(root)? {
        Some(prepared) => prepared
            .repo()
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf()),
        None => root.to_path_buf(),
    };
    let (key, own) = (folder_key(&home), folder_key(root));
    let dir = personal_dir(&home)?;
    if key != own {
        adopt_folder_connection(&personal_dir(root)?, &dir, &own, &key)?;
    }
    Ok((dir.join(crate::remote::CONNECTION_FILE), key))
}

/// Held while a repository's connection file and secret change together, so the address kept
/// never ends up beside another connection's secret — two folders of one repository are looked
/// up at the same moment, and either may be the one that takes its earlier connection over.
fn connection_change() -> std::sync::MutexGuard<'static, ()> {
    static CHANGE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    CHANGE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Moves a folder's own remote connection (file and secret) to its repository's place, when the
/// repository has none. The file moves first and the secret after it, under
/// [`connection_change`]: a repository place another folder filled first is left as it is, with
/// its own secret.
fn adopt_folder_connection(
    folder_dir: &Path,
    repo_dir: &Path,
    folder_key: &str,
    repo_key: &str,
) -> Result<(), String> {
    let old = folder_dir.join(crate::remote::CONNECTION_FILE);
    let new = repo_dir.join(crate::remote::CONNECTION_FILE);
    if !old.is_file() || new.symlink_metadata().is_ok() {
        return Ok(());
    }
    let _change = connection_change();
    if !old.is_file() || new.symlink_metadata().is_ok() {
        return Ok(());
    }
    if !crate::state_dir::writable(repo_dir).map_err(|e| e.to_string())? {
        return Ok(());
    }
    std::fs::create_dir_all(repo_dir).map_err(|e| e.to_string())?;
    match crate::fs_ops::rename(&old, &new) {
        Ok(()) => {}
        // Filled meanwhile from outside this process: that connection stands, with its secret.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e.to_string()),
    }
    if let Some(secret) = crate::remote::secret(folder_key) {
        crate::remote::set_secret(repo_key, &secret)?;
        crate::remote::clear_secret(folder_key)?;
    }
    Ok(())
}

/// The remote this folder exchanges notes with, as this machine remembers it — the one of the
/// repository it records into ([`remote_place`]).
pub fn remote_connection(root: String) -> Result<Option<crate::remote::Connection>, String> {
    let (path, _) = remote_place(Path::new(&root))?;
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
    let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
    // After `prepare`: the place is the repository's, and a folder with none has one now.
    let (path, key) = remote_place(root_p)?;
    let dir = path
        .parent()
        .ok_or("cannot find a place to keep settings")?;
    if !crate::state_dir::writable(dir).map_err(|e| e.to_string())? {
        return Err(NEWER_SETTINGS.into());
    }
    let credentials = crate::git_transport::Credentials {
        username: username.clone(),
        secret: secret.clone(),
    };
    crate::remote::fetch(prepared.repo(), &url, Some(credentials))
        .map_err(|e| e.message().to_string())?;

    let connection = crate::remote::Connection { url, username };
    let text = serde_json::to_string_pretty(&connection).map_err(|e| e.to_string())?;
    let _change = connection_change();
    crate::remote::set_secret(&key, &secret)?;
    atomic_write_beside(&path, &text).map_err(|e| e.to_string())
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

/// Forgets this folder's remote on this machine — for every folder of its repository, since they
/// share it. Nothing on the remote changes.
pub fn disconnect_remote(root: String) -> Result<(), String> {
    let (path, key) = remote_place(Path::new(&root))?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    crate::remote::clear_secret(&key)
}

/// One exchange with this folder's remote: takes in what arrived where it can, and sends what
/// was recorded here.
pub fn sync_remote(root: String, locks: &NoteLocks) -> Result<crate::remote::Exchange, String> {
    let root_p = Path::new(&root);
    let connection = remote_connection(root.clone())?
        .ok_or_else(|| "this folder is not connected to a remote".to_string())?;
    let (_, key) = remote_place(root_p)?;
    // Folders of one repository share its remote: one exchange at a time for the repository, so
    // two opened together do not fetch, take in and send over each other.
    let turn = exchange_turn(&key);
    let _turn = turn.lock().unwrap_or_else(|p| p.into_inner());
    let credentials = crate::remote::secret(&key).map(|secret| crate::git_transport::Credentials {
        username: connection.username.clone(),
        secret,
    });
    let prepared = crate::git_engine::prepare(root_p).map_err(|e| e.message().to_string())?;
    crate::remote::exchange(&prepared, root_p, &connection.url, credentials, locks)
}

/// The turn for exchanges of the repository whose remote is kept under `key`.
fn exchange_turn(key: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    static TURNS: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let mut turns = TURNS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    turns.entry(key.to_string()).or_default().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{commit_notes, tests::seed_note};
    use tempfile::TempDir;

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
    fn folders_of_one_repository_share_its_remote() {
        use crate::git_transport::test_server::{SECRET, USER};
        let tmp = TempDir::new().unwrap();
        let Some(url) = remote_for_test(tmp.path()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let locks = NoteLocks::default();
        let repo = tmp.path().join("work");
        git2::Repository::init(&repo).unwrap();
        let (a, b) = (repo.join("journal"), repo.join("manual"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a_s, b_s) = (
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        );

        connect_remote(a_s.clone(), url.clone(), USER.into(), SECRET.into()).unwrap();
        let shared = Some(crate::remote::Connection {
            url: url.clone(),
            username: USER.into(),
        });
        assert_eq!(remote_connection(b_s.clone()).unwrap(), shared);
        assert!(crate::remote::secret(&folder_key(&repo)).is_some());
        assert!(crate::remote::secret(&folder_key(&a)).is_none());

        // Recorded in one folder, sent from the other: one repository, one record of what the
        // remote holds — both say the same about being backed up.
        seed_note(&b, "b.md", "# B\n");
        commit_notes(
            b_s.clone(),
            vec![b.join("b.md").to_string_lossy().to_string()],
            "b".into(),
        )
        .unwrap();
        assert!(!notes_backed_up(a_s.clone()).unwrap());
        assert!(!notes_backed_up(b_s.clone()).unwrap());
        assert!(sync_remote(a_s.clone(), &locks).unwrap().sent);
        assert!(notes_backed_up(a_s.clone()).unwrap());
        assert!(notes_backed_up(b_s.clone()).unwrap());

        disconnect_remote(b_s.clone()).unwrap();
        assert_eq!(remote_connection(a_s.clone()).unwrap(), None);
        assert!(crate::remote::secret(&folder_key(&repo)).is_none());
    }

    #[test]
    fn a_connection_kept_with_a_folder_inside_a_repository_moves_to_the_repository() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("work");
        git2::Repository::init(&repo).unwrap();
        let (a, b) = (repo.join("journal"), repo.join("manual"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let earlier = |folder: &Path, url: &str| {
            let dir = personal_dir(folder).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            let c = crate::remote::Connection {
                url: url.into(),
                username: "me".into(),
            };
            std::fs::write(
                dir.join(crate::remote::CONNECTION_FILE),
                serde_json::to_string(&c).unwrap(),
            )
            .unwrap();
            crate::remote::set_secret(&folder_key(folder), url).unwrap();
        };
        earlier(&a, "https://a.example/notes.git");
        earlier(&b, "https://b.example/notes.git");

        let a_s = a.to_string_lossy().to_string();
        let got = remote_connection(a_s.clone()).unwrap().unwrap();
        assert_eq!(got.url, "https://a.example/notes.git");
        assert!(!personal_dir(&a)
            .unwrap()
            .join(crate::remote::CONNECTION_FILE)
            .exists());
        assert_eq!(
            crate::remote::secret(&folder_key(&repo)).as_deref(),
            Some("https://a.example/notes.git")
        );
        assert!(crate::remote::secret(&folder_key(&a)).is_none());

        // The repository has one now: the other folder's earlier one is left as it was.
        let b_s = b.to_string_lossy().to_string();
        assert_eq!(
            remote_connection(b_s).unwrap().unwrap().url,
            "https://a.example/notes.git"
        );
        assert!(personal_dir(&b)
            .unwrap()
            .join(crate::remote::CONNECTION_FILE)
            .exists());

        disconnect_remote(a_s).unwrap();
        crate::remote::clear_secret(&folder_key(&b)).unwrap();
    }

    #[test]
    fn folders_looked_up_together_keep_one_connection_with_its_own_secret() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("work");
        git2::Repository::init(&repo).unwrap();
        let folders: Vec<_> = (0..4).map(|i| repo.join(format!("f{i}"))).collect();
        for (i, folder) in folders.iter().enumerate() {
            std::fs::create_dir_all(folder).unwrap();
            let dir = personal_dir(folder).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            let url = format!("https://f{i}.example/notes.git");
            let c = crate::remote::Connection {
                url: url.clone(),
                username: "me".into(),
            };
            std::fs::write(
                dir.join(crate::remote::CONNECTION_FILE),
                serde_json::to_string(&c).unwrap(),
            )
            .unwrap();
            // Each folder's secret names its own address, so a mismatch shows.
            crate::remote::set_secret(&folder_key(folder), &url).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(folders.len()));
        let handles: Vec<_> = folders
            .iter()
            .map(|f| {
                let (f, b) = (f.to_string_lossy().to_string(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    remote_connection(f).unwrap()
                })
            })
            .collect();
        let seen: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let kept = seen[0].clone().unwrap();
        assert!(seen.iter().all(|c| c.as_ref() == Some(&kept)), "{seen:?}");
        assert_eq!(
            crate::remote::secret(&folder_key(&repo)).as_deref(),
            Some(kept.url.as_str()),
            "the secret kept is the one of the address kept"
        );

        crate::remote::clear_secret(&folder_key(&repo)).unwrap();
        for folder in &folders {
            crate::remote::clear_secret(&folder_key(folder)).unwrap();
        }
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
}

//! The main window and the web view that draws it.
//!
//! The window is made here rather than from the configuration on its own (`create: false`), so that
//! what has to hold from its first moment holds:
//!
//! - **Nothing typed is remembered by the web view.** Autofill is off on the window. On Windows the
//!   web view's own profile — outside the notes folder and outside anything this app keeps — is
//!   cleared of what an earlier version may have remembered, and the copies of that profile the
//!   runtime takes when it updates itself are removed.
//! - **The window stays on the app.** A link, a script or a redirect cannot take the window to
//!   another address (Windows). A web address the person clicks opens in their browser instead.
//! - **A page whose process ended comes back.** When the process drawing the page ends, the window
//!   would stay blank; it is reloaded once per failure and the app's log says why (Windows).
//!
//! Before any of that, [`require_runtime`] says in words when the web view runtime is missing, where
//! the framework would otherwise stop with a message of its own.

use tauri::App;

/// The window's label, as the configuration names it.
pub const MAIN: &str = "main";

/// Stops with a message the person can act on when the web view runtime is missing (Windows — on
/// other systems the web view is part of the operating system).
pub fn require_runtime() {
    #[cfg(windows)]
    if let tauri_kit_webview::Runtime::Missing = tauri_kit_webview::runtime() {
        tauri_kit_webview::alert(
            "Textree cannot open its window",
            "Textree draws its window with the Microsoft Edge WebView2 Runtime, which is not \
             installed on this PC. Install it from Microsoft (search for \"WebView2 Runtime\"), \
             then start Textree again.",
        );
        std::process::exit(1);
    }
}

/// Makes the main window from its configuration, with the web view settings above.
pub fn create(app: &mut App) -> tauri::Result<()> {
    let Some(config) = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN)
        .cloned()
    else {
        log::error!("the configuration has no window labelled {MAIN}");
        return Ok(());
    };
    let window = tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?
        .general_autofill_enabled(false)
        .build()?;
    #[cfg(windows)]
    windows::hold(app, &window);
    #[cfg(not(windows))]
    let _ = window;
    Ok(())
}

/// The addresses the window may show: the app as built, and in a development build the dev server
/// too.
#[cfg(windows)]
fn own_origins() -> Vec<&'static str> {
    let mut origins = vec!["http://tauri.localhost", "https://tauri.localhost"];
    if cfg!(debug_assertions) {
        origins.push("http://localhost:1420");
    }
    origins
}

#[cfg(windows)]
mod windows {
    use super::own_origins;
    use tauri::{App, Manager, WebviewWindow};
    use tauri_kit_webview::{
        failure, forget_form_entries, lock_navigation, remove_profile_snapshots, web_address,
        Attempt, Kind, NavigationLock, Origin,
    };

    pub(super) fn hold(app: &App, window: &WebviewWindow) {
        if let Ok(data_dir) = app.path().app_local_data_dir() {
            if let Err(e) = remove_profile_snapshots(&data_dir) {
                log::warn!("[webview] could not remove the profile's update snapshots: {e}");
            }
        }
        let handle = app.handle().clone();
        let lock = NavigationLock::new(
            own_origins()
                .into_iter()
                .filter_map(|o| Origin::parse(o).ok()),
        );
        let result = window.with_webview(move |webview| {
            let controller = webview.controller();
            if let Err(e) = forget_form_entries(&controller) {
                log::warn!("[webview] could not clear remembered form entries: {e}");
            }
            let opener = handle.clone();
            match lock_navigation(&controller, lock, move |attempt| leave(&opener, attempt)) {
                Ok(enforced) => log::info!(
                    "[webview] window held to its own origins (frames before request: {}, other schemes: {})",
                    enforced.frame_requests,
                    enforced.external_schemes
                ),
                Err(e) => log::warn!("[webview] could not hold the window to its own origins: {e}"),
            }
            let reloader = controller.clone();
            if let Err(e) = failure::watch(&controller, move |failed| {
                log::warn!(
                    "[webview] a web view process ended: {:?} ({:?}, exit code {})",
                    failed.process,
                    failed.cause,
                    failed.exit_code
                );
                if failed.process == failure::Process::Render {
                    if let Err(e) = failure::reload(&reloader) {
                        log::warn!("[webview] could not reload the page: {e}");
                    }
                }
            }) {
                log::warn!("[webview] cannot tell when a web view process ends: {e}");
            }
        });
        if let Err(e) = result {
            log::warn!("[webview] could not reach the window's web view: {e}");
        }
    }

    /// A way out of the window the lock refused. Only a web address the person chose themselves goes
    /// on, to their browser — never one a page opened on its own, which could carry data out in it.
    fn leave(_app: &tauri::AppHandle, attempt: Attempt) {
        let chosen = attempt.user_initiated == Some(true)
            && matches!(attempt.kind, Kind::Navigation | Kind::NewWindow);
        match web_address(&attempt.uri) {
            Some(url) if chosen => {
                if let Err(e) = tauri_plugin_opener::open_url(url, None::<&str>) {
                    log::warn!("[webview] could not open a web address in the browser: {e}");
                }
            }
            _ => log::info!(
                "[webview] the window stayed on the app ({:?})",
                attempt.kind
            ),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::own_origins;
    use tauri_kit_webview::{Decision, NavigationLock, Origin};

    fn lock() -> NavigationLock {
        NavigationLock::new(own_origins().into_iter().map(|o| Origin::parse(o).unwrap()))
    }

    #[test]
    fn the_app_itself_is_allowed() {
        assert_eq!(
            lock().decide("http://tauri.localhost/index.html"),
            Decision::Allow
        );
        assert_eq!(lock().decide("https://tauri.localhost/"), Decision::Allow);
    }

    #[test]
    fn any_other_address_is_refused() {
        for url in [
            "https://example.com/",
            "http://tauri.localhost.example.com/",
            "http://localhost:1421/",
            "file:///C:/notes/a.md",
        ] {
            assert!(matches!(lock().decide(url), Decision::Refuse(_)), "{url}");
        }
    }
}

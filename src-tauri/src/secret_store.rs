//! Where this application keeps its secrets: the OS credential store, through
//! `tauri-kit-credentials`.
//!
//! Secrets live under one fixed entry per kind, so anything that writes or clears an entry writes
//! or clears *the* entry — the one the installed app uses. A unit test that ends by clearing it
//! destroys the user's stored token, and a development build would share credentials with the copy
//! of the app the user actually relies on. Each kind of build therefore gets its own entry:
//!
//! | build | service name |
//! |---|---|
//! | `cargo test` | `<service>.test` |
//! | `cargo build` / `tauri dev` | `<service>.dev` |
//! | shipped release | `<service>` |
//!
//! The kind of build is decided at compile time, by `build_kind!()` written here — in this crate,
//! where `cfg!(test)` describes this application rather than the library.

use tauri_kit_credentials::{build_kind, Credentials};

/// The secrets kept under `service`, as seen by this kind of build.
pub(crate) fn credentials(service: &str) -> Credentials {
    Credentials::new(service, build_kind!())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_suite_never_opens_the_entry_the_installed_app_uses() {
        let base = "com.example.secret";
        assert_eq!(credentials(base).service(), "com.example.secret.test");
    }

    #[test]
    fn a_release_build_keeps_the_name_installed_apps_already_store_under() {
        // Changing how the shipped name is formed would orphan every secret users already have.
        let release = Credentials::new(
            "com.example.secret",
            tauri_kit_credentials::BuildKind::Release,
        );
        assert_eq!(release.service(), "com.example.secret");
    }

    #[test]
    fn a_test_run_is_kept_separate_from_a_development_session() {
        // Test builds are debug builds, so the test entry has to win — otherwise running the merge
        // gate would clear the token a `tauri dev` session had signed in with.
        let dev = Credentials::new("com.example.secret", tauri_kit_credentials::BuildKind::Dev);
        assert_eq!(dev.service(), "com.example.secret.dev");
        assert_ne!(credentials("com.example.secret").service(), dev.service());
    }
}

pub(crate) mod alternatives;
pub(crate) mod byo_secret;
pub(crate) mod cloud_auth;
pub(crate) mod cloud_publish;
pub(crate) mod commands;
pub(crate) mod dev_env;
pub(crate) mod drafts;
pub(crate) mod fs_ops;
#[cfg(test)]
mod gate_deletion_recoverable;
#[cfg(test)]
mod gate_foreground_latency;
#[cfg(test)]
mod gate_history_kept;
#[cfg(test)]
mod gate_staging_untouched;
#[cfg(test)]
mod gate_vault_untouched;
#[cfg(test)]
mod gate_write_interrupted;
pub(crate) mod git_engine;
pub(crate) mod git_transport;
pub(crate) mod host;
pub(crate) mod note_locks;
pub(crate) mod note_refs;
pub(crate) mod pathsafe;
pub(crate) mod publish;
pub(crate) mod publish_assembly;
pub(crate) mod publish_scope;
pub(crate) mod publish_secret;
pub(crate) mod remote;
pub(crate) mod search;
pub(crate) mod secret_store;
pub(crate) mod state_dir;
pub(crate) mod stranded;
mod telemetry;
pub(crate) mod tree_commands;
pub(crate) mod vault;
pub mod vault_paths;
pub(crate) mod watcher;
pub(crate) mod workspace;

use host::HostHandle;
use search::IndexHandle;
use std::sync::Arc;
use watcher::WatcherHandle;

/// Brings the main window back in front of the person: restored if minimized, shown, focused.
#[cfg(not(debug_assertions))]
fn show_main_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Content-free crash signal: capture only our own source location, never the panic payload
    // (which could interpolate user data). Chains to the previous hook so default logging/abort
    // behaviour is preserved.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        // Appends one line to a file; sent on a later launch, since this process is ending.
        telemetry::record(telemetry::panic_report(&location));
        prev_hook(info);
    }));

    let builder = tauri::Builder::default();
    // First, so a second launch stops before anything else starts. Release builds only: a
    // development or test build shares the installed app's identifier, and would otherwise just
    // bring an installed copy that happens to be running to the front and quit.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_main_window(app);
    }));
    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("textree".into()),
                    },
                ))
                .level(log::LevelFilter::Info)
                // Rotation: keep at most one rotated file (~5 MB ceiling).
                .max_file_size(5_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            {
                use tauri::Manager;
                if let Ok(log_dir) = app.path().app_log_dir() {
                    telemetry::start(&log_dir);
                }
            }
            // Log startup here (after plugin init) so the logger is already wired.
            log::info!("textree starting (v{})", env!("CARGO_PKG_VERSION"));
            #[cfg(windows)]
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())?;
            // Spawn the local AI host when TEXTREE_HOST_EXE is set (development and test builds
            // only — see `dev_env`). Without it the handle stays Unavailable until the person
            // turns the helper on — graceful degradation.
            if let Some(exe) = dev_env::var("TEXTREE_HOST_EXE") {
                use tauri::Manager;
                let handle = app.state::<std::sync::Arc<HostHandle>>().inner().clone();
                if let Ok(log_dir) = app.path().app_log_dir() {
                    host::spawn_host(handle, exe, log_dir, Vec::new(), "local".to_string());
                }
            }
            Ok(())
        })
        // What the app writes to notes, shared by the watch (which leaves it out) and write_note.
        .manage(tauri_kit_watch::OwnWrites::new())
        .manage(Arc::new(note_locks::NoteLocks::default()))
        .manage(WatcherHandle::default())
        .manage(Arc::new(IndexHandle::default()))
        .manage(Arc::new(HostHandle::default()))
        .invoke_handler(tauri::generate_handler![
            tree_commands::open_vault,
            tree_commands::register_root,
            commands::list_tree,
            commands::read_note,
            commands::write_note,
            tree_commands::commit_notes,
            tree_commands::note_versions,
            tree_commands::note_version_text,
            tree_commands::deleted_notes,
            tree_commands::restore_deleted,
            tree_commands::restore_version,
            tree_commands::list_alternatives,
            tree_commands::start_alternative,
            tree_commands::alternative_text,
            tree_commands::alternative_draft,
            tree_commands::write_alternative_draft,
            tree_commands::add_alternative_version,
            tree_commands::use_alternative,
            tree_commands::set_aside_alternative,
            tree_commands::move_state_out_of_vault,
            tree_commands::create_note,
            tree_commands::create_untitled_note,
            tree_commands::create_note_with_content,
            tree_commands::create_folder,
            tree_commands::promote_node,
            tree_commands::delete_node,
            tree_commands::rename_node,
            tree_commands::rename_note_unique,
            tree_commands::move_node,
            tree_commands::adopt_node,
            tree_commands::save_attachment,
            tree_commands::read_sidecar,
            tree_commands::write_sidecar,
            tree_commands::prepare_sidecar,
            tree_commands::set_aside_sidecar,
            commands::keep_stranded,
            commands::forget_stranded,
            commands::list_stranded,
            commands::search_content,
            tree_commands::rebuild_index,
            tree_commands::publish_preview,
            tree_commands::publish_choices,
            tree_commands::set_publish_choice,
            tree_commands::remote_connection,
            tree_commands::connect_remote,
            tree_commands::disconnect_remote,
            tree_commands::notes_backed_up,
            tree_commands::sync_remote,
            tree_commands::publish_site,
            tree_commands::publish_to_cloud,
            cloud_auth::connect_publish,
            tree_commands::ensure_default_vault,
            commands::open_log_dir,
            host::host_status,
            host::semantic_search,
            host::prepare_ai_model,
            host::stop_host,
            host::ask,
            host::cancel_ask,
            host::prepare_generation,
            host::test_byo_connection,
            byo_secret::set_byo_api_key,
            byo_secret::clear_byo_api_key,
            byo_secret::has_byo_api_key,
            publish_secret::clear_publish_token,
            publish_secret::has_publish_token,
            host::restart_ai_host
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                log::info!("textree shutting down");
                use tauri::Manager;
                let handle = app_handle.state::<std::sync::Arc<HostHandle>>();
                host::shutdown_host(&handle);
            }
        });
}

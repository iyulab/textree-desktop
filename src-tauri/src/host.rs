use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use tauri_kit_sidecar::loopback::{Client, Loopback, LoopbackOptions};

// Every request to the host goes through a `Client`: it carries the per-run token the host serves
// only to this app, so none can leave the token out.
use tauri_kit_sidecar::{LineReadiness, Output, Sidecar, Sink};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostStatus {
    Starting,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSnapshot {
    pub phase: String,
    pub overall_percent: f64,
    pub bytes_downloaded: i64,
    pub total_bytes: i64,
    pub file_index: i32,
    pub file_count: i32,
}

#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    // Mirrors the /health wire contract alongside its sibling fields and is
    // asserted by the parse_health test; Rust derives readiness from the
    // boolean flags below, so it is unread in non-test builds.
    #[allow(dead_code)]
    pub status: String,
    #[serde(rename = "embedderReady")]
    pub embedder_ready: bool,
    #[serde(rename = "generatorReady", default)]
    pub generator_ready: bool,
    #[serde(rename = "generatorError", default)]
    pub generator_error: Option<String>,
    #[serde(rename = "embedderError", default)]
    pub embedder_error: Option<String>,
    #[serde(rename = "embedderDownload", default)]
    pub embedder_download: Option<DownloadSnapshot>,
    #[serde(rename = "generatorDownload", default)]
    pub generator_download: Option<DownloadSnapshot>,
}

pub fn parse_health(body: &str) -> Option<HealthResponse> {
    serde_json::from_str(body).ok()
}

// ---------------------------------------------------------------------------
// HostHandle and accessors
// ---------------------------------------------------------------------------

struct HostStatusCell(HostStatus);
impl Default for HostStatusCell {
    fn default() -> Self {
        HostStatusCell(HostStatus::Unavailable)
    }
}

/// The environment variable the host reads its token from (`HostToken.Setting` on the .NET side).
const HOST_TOKEN_ENV: &str = "TEXTREE_HOST_TOKEN";

/// What the host prints, followed by its port, once it listens (`HostToken.ReadyPrefix`).
const HOST_READY_PREFIX: &str = "textree-host ready port=";

/// How long a starting host may take to say it listens. Models load afterwards, in the background;
/// this covers the runtime starting — the first run of a single-file build unpacks itself first.
const HOST_READY_TIMEOUT: Duration = Duration::from_secs(60);

fn host_options(log_dir: &Path) -> LoopbackOptions {
    let mut options = LoopbackOptions::new(HOST_TOKEN_ENV, HOST_READY_PREFIX);
    options.ready_timeout = HOST_READY_TIMEOUT;
    // host-out.log / host-err.log land in app_log_dir so they sit next to textree.log and the
    // "Open log folder" palette command exposes them in one place.
    options.stdout = Some(log_dir.join("host-out.log"));
    options.stderr = log_dir.join("host-err.log").into();
    options
}

#[derive(Default)]
pub struct HostHandle {
    status: Mutex<HostStatusCell>,
    endpoint: Mutex<Option<Client>>,
    /// The running host and everything it started, stopped together (see [`shutdown_host`]).
    child: Mutex<Option<Sidecar>>,
    generation: AtomicU64,
    /// Separate single-flight counter for `ask` streams.
    /// Must NOT share `generation` — that counter drives the health-poll thread
    /// (spawn_host increments it); mixing them would cancel in-flight asks on
    /// every host respawn, and a new ask would invalidate the health poller.
    ask_generation: AtomicU64,
    current_vault: Mutex<Option<String>>,
    /// Read-only tracking of the host's generatorReady flag (from /health).
    /// Does NOT gate the Ready transition — embedder_ready controls that.
    generator_ready: Mutex<bool>,
    /// Last-known generator load error from /health (None = no error). Read-only tracking,
    /// surfaced via host_status so the frontend can show a failure instead of hanging on
    /// "preparing". Cleared optimistically by prepare_generation, refreshed every poll.
    generator_error: Mutex<Option<String>>,
    /// Last-known embedder load error from /health (None = no error).
    embedder_error: Mutex<Option<String>>,
    /// Last-known embedder download progress snapshot from /health (None = not downloading).
    embedder_download: Mutex<Option<DownloadSnapshot>>,
    /// Last-known generator download progress snapshot from /health (None = not downloading).
    generator_download: Mutex<Option<DownloadSnapshot>>,
    /// Which text-generation backend the current (or most recent) spawn used: "local" for the
    /// bundled model, or the BYO preset string ("ollama"/"gpustack"/"custom"). Surfaced via
    /// host_status so Settings can show "Running on: ...".
    active_provider: Mutex<String>,
}

impl HostHandle {
    pub fn status(&self) -> HostStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }
    fn set_status(&self, s: HostStatus) {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).0 = s;
    }
    /// The host's exit status, if the process has exited. Does not wait.
    fn child_exit(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_mut()
            .and_then(|child| child.try_status().ok().flatten())
    }
    /// Atomically claim the right to spawn the host: transition to `Starting` only if no spawn is
    /// already in flight (`Starting`) or up (`Ready`), all under a single lock so the check-and-set
    /// is indivisible. The previous guard read the status and set `Starting` in two separate lock
    /// acquisitions with a port-alloc + process-spawn gap between them, letting two concurrent
    /// callers (mount auto-spawn racing the `?`-enable, or a dev eager-spawn racing a manual
    /// trigger) both pass and double-spawn — orphaning the first child (clobbered in `child`) and
    /// leaking its port. Returns true to the single winner, false to everyone else.
    fn try_begin_spawn(&self) -> bool {
        let mut cell = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(cell.0, HostStatus::Starting | HostStatus::Ready) {
            return false;
        }
        cell.0 = HostStatus::Starting;
        true
    }
    pub fn endpoint(&self) -> Option<Client> {
        self.endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn set_current_vault(&self, vault: String) {
        *self.current_vault.lock().unwrap_or_else(|e| e.into_inner()) = Some(vault);
    }
    pub fn current_vault(&self) -> Option<String> {
        self.current_vault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// Whether `vault` is the folder open now, however its path is spelled.
    pub fn is_current_vault(&self, vault: &str) -> bool {
        use crate::search::vault_identity;
        self.current_vault().is_some_and(|open| {
            vault_identity(Path::new(&open)) == vault_identity(Path::new(vault))
        })
    }
    /// Bump the ask-stream generation and return the new value.
    /// Used by `ask` for single-flight cancellation (independent of health-poll `generation`).
    pub fn bump_ask_generation(&self) -> u64 {
        self.ask_generation.fetch_add(1, Ordering::SeqCst) + 1
    }
    /// Read the current ask generation (used inside spawn_blocking to detect cancellation).
    pub fn ask_generation(&self) -> u64 {
        self.ask_generation.load(Ordering::SeqCst)
    }
    /// Returns the last-known value of the host's generatorReady flag.
    /// Updated on each successful /health poll; does not gate the Ready transition.
    pub fn generator_ready(&self) -> bool {
        *self
            .generator_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    fn set_generator_ready(&self, v: bool) {
        *self
            .generator_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }
    /// Returns the last-known generator load error (None = healthy / still preparing).
    pub fn generator_error(&self) -> Option<String> {
        self.generator_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn set_generator_error(&self, v: Option<String>) {
        *self
            .generator_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }
    /// Returns the last-known embedder load error (None = healthy / still preparing).
    pub fn embedder_error(&self) -> Option<String> {
        self.embedder_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn set_embedder_error(&self, v: Option<String>) {
        *self
            .embedder_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }
    /// Returns the last-known embedder download progress (None = not downloading/loading).
    pub fn embedder_download(&self) -> Option<DownloadSnapshot> {
        self.embedder_download
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn set_embedder_download(&self, v: Option<DownloadSnapshot>) {
        *self
            .embedder_download
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }
    /// Returns the last-known generator download progress (None = not downloading/loading).
    pub fn generator_download(&self) -> Option<DownloadSnapshot> {
        self.generator_download
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn set_generator_download(&self, v: Option<DownloadSnapshot>) {
        *self
            .generator_download
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }
    pub fn active_provider(&self) -> String {
        self.active_provider
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Spawn + health poll
// ---------------------------------------------------------------------------

const HEALTH_CEILING: Duration = Duration::from_secs(15 * 60); // first-run model download
const FAST_PATH: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// After the host is Ready we keep polling for its lifetime only to observe the lazily-loaded
/// generator flip generatorReady; back off so steady-state polling does not flood the host log
/// (Kestrel logs ~2 lines per /health request). ≤10s gate latency is dwarfed by model load.
const READY_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Builds the extra env vars for a BYO-configured host spawn. `preset=None` (bundled local model,
/// the default) yields an empty vec — spawn_host's caller passes that straight through unchanged
/// from today's behavior. `api_key` is looked up by the caller from the OS keychain (byo_secret::
/// get_api_key) — this function only assembles what it's given, kept pure for unit testing.
fn resolve_byo_env(
    preset: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
) -> Vec<(String, String)> {
    let Some(preset) = preset.filter(|p| !p.trim().is_empty()) else {
        return Vec::new();
    };
    let mut env = vec![("TEXTREE_BYO_PRESET".to_string(), preset)];
    if let Some(base_url) = base_url.filter(|b| !b.is_empty()) {
        env.push(("TEXTREE_BYO_BASE_URL".to_string(), base_url));
    }
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        env.push(("TEXTREE_BYO_MODEL".to_string(), model));
    }
    if let Some(api_key) = api_key.filter(|k| !k.is_empty()) {
        env.push(("TEXTREE_BYO_API_KEY".to_string(), api_key));
    }
    env
}

/// Spawn the host on a fresh loopback port and begin health polling on a
/// background thread. Never panics; on any failure the handle stays Unavailable.
pub fn spawn_host(
    handle: Arc<HostHandle>,
    exe: String,
    log_dir: std::path::PathBuf,
    extra_env: Vec<(String, String)>,
    active_provider: String,
) {
    // Idempotent: never double-spawn. A concurrent caller (mount auto-spawn + ? enable, or a
    // dev eager-spawn racing a manual trigger) must not orphan a child or clobber the port.
    // try_begin_spawn atomically claims the spawn (transition → Starting under one lock); only the
    // single winner proceeds. The failure paths below roll the claim back to Unavailable.
    if !handle.try_begin_spawn() {
        return;
    }
    let mut cmd = Command::new(&exe);
    // The helper writes its failures into this process's reports folder; this process verifies
    // and sends them. The helper is never given a connection string.
    cmd.env(
        crate::telemetry::HOST_REPORTS_VAR,
        crate::telemetry::reports_dir(&log_dir),
    );
    for (k, v) in &extra_env {
        cmd.env(k, v);
    }
    let options = host_options(&log_dir);
    *handle
        .active_provider
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = active_provider;
    // Status was already claimed as Starting by try_begin_spawn at the top.
    let my_gen = handle.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let h = handle.clone();
    // Starting waits for the host to say which port it took, so it runs off the caller's thread.
    std::thread::spawn(move || {
        // A fresh token per run, handed over in the environment; the host picks a free port itself
        // and says which, so no other program can take the port between choosing and binding it.
        // No console window on Windows, and the host joins a job object (a process group
        // elsewhere), so whatever it starts is stopped with it — also when this app crashes. The
        // bundled host is published self-contained single-file (assemble-host-sidecar.ps1); in dev,
        // TEXTREE_HOST_EXE may point at a framework-dependent build using the ambient runtime.
        let (child, endpoint) = match Loopback::start(cmd, &options) {
            Ok(started) => {
                let client = started.client().clone();
                (started.into_parts().0, client)
            }
            Err(e) => {
                log::error!("[host] start failed: {e}");
                if h.generation.load(Ordering::SeqCst) == my_gen {
                    h.set_status(HostStatus::Unavailable);
                }
                return;
            }
        };
        // Stopped while it was starting: what the stop could not see yet goes now.
        if h.generation.load(Ordering::SeqCst) != my_gen {
            let _ = child.shutdown(SHUTDOWN_GRACE);
            return;
        }
        *h.endpoint.lock().unwrap_or_else(|e| e.into_inner()) = Some(endpoint.clone());
        *h.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        poll_health(h, endpoint, my_gen);
    });
}

/// Per-poll decision for the health loop. Pure (no I/O) so it is unit-tested.
/// `already_ready` = the host previously reached Ready this spawn; once true we keep polling
/// for the host's lifetime to refresh generatorReady (it loads lazily, long after
/// embedder_ready) and the startup ceiling no longer applies.
#[derive(Debug, PartialEq, Eq)]
enum PollAction {
    /// First observation of embedder_ready: transition to Ready (+ reindex) once, then keep polling.
    BecomeReady,
    /// Stay in the loop and poll again (still starting within the ceiling, or already Ready).
    KeepPolling,
    /// Never reached Ready and the startup ceiling elapsed: mark Unavailable and stop.
    GiveUp,
    /// The process is gone — during startup or after Ready. Mark Unavailable and stop: no amount
    /// of polling will bring it back, and until now a crash looked like a slow start for up to the
    /// full ceiling, or left a dead host reported as Ready.
    Exited,
}

fn poll_action(
    exited: bool,
    embedder_ready: bool,
    already_ready: bool,
    ceiling_exceeded: bool,
) -> PollAction {
    if exited {
        PollAction::Exited
    } else if already_ready {
        // Host is up; keep refreshing generatorReady indefinitely (ceiling no longer applies).
        PollAction::KeepPolling
    } else if embedder_ready {
        PollAction::BecomeReady
    } else if ceiling_exceeded {
        PollAction::GiveUp
    } else {
        PollAction::KeepPolling
    }
}

/// Poll fast (1s) while starting OR while any model is actively downloading (so generator
/// download progress — which begins after embedder_ready — updates live); back off to 10s only
/// in steady state (host ready and nothing downloading) to keep the host log quiet.
fn poll_interval(ready: bool, any_download_active: bool) -> Duration {
    if ready && !any_download_active {
        READY_POLL_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

fn poll_health(handle: Arc<HostHandle>, host: Client, my_gen: u64) {
    let start = Instant::now();
    let mut announced = false;
    let mut ready = false;
    loop {
        // Stay at 1s whenever any model is actively downloading (e.g. generator lazy-download
        // begins after embedder_ready, while ready==true). First iteration: any_dl=false but
        // ready=false → 1s anyway (harmless 1-poll lag before download state is observed).
        let any_dl = handle.embedder_download().is_some() || handle.generator_download().is_some();
        // When a generator download begins while ready==true, any_dl was false on this iteration,
        // so the sleep here can last up to one READY_POLL_INTERVAL (≤10s) before the cadence drops
        // to 1s; the generic "preparing" UI in the front-end covers that brief window.
        std::thread::sleep(poll_interval(ready, any_dl));
        // A newer spawn or a shutdown invalidates this poll thread.
        if handle.generation.load(Ordering::SeqCst) != my_gen {
            return;
        }
        // Checked before the request: a dead host would otherwise cost a full request timeout per
        // tick, and its absence would read as "not ready yet".
        let exit = handle.child_exit();
        if let Some(status) = exit {
            log::warn!("[host] process exited ({status}); marking unavailable");
        }
        let health = if exit.is_some() {
            None
        } else {
            host.within(Duration::from_secs(5))
                .get("/health")
                .ok()
                .filter(|resp| resp.is_success())
                .and_then(|resp| parse_health(&resp.body))
        };
        // The request can outlast a restart (up to its timeout): what the old host said must not
        // overwrite what the new one's poller reports.
        if handle.generation.load(Ordering::SeqCst) != my_gen {
            return;
        }

        if let Some(h) = &health {
            // Refresh generatorReady on EVERY poll for the host's lifetime: the generator loads
            // lazily (first /chat or /prepare-generation), long after embedder_ready. Previously
            // the loop returned on embedder_ready, freezing generatorReady=false forever, so the
            // frontend chat/ask gate never advanced past "preparing".
            handle.set_generator_ready(h.generator_ready);
            handle.set_generator_error(h.generator_error.clone());
            handle.set_embedder_download(h.embedder_download.clone());
            handle.set_generator_download(h.generator_download.clone());
            handle.set_embedder_error(h.embedder_error.clone());
        }
        let embedder_ready = health.as_ref().map(|h| h.embedder_ready).unwrap_or(false);
        let ceiling_exceeded = start.elapsed() >= HEALTH_CEILING;
        match poll_action(exit.is_some(), embedder_ready, ready, ceiling_exceeded) {
            PollAction::BecomeReady => {
                ready = true;
                handle.set_status(HostStatus::Ready);
                // Re-trigger the reindex the open vault missed while the host was Starting
                // (lazy spawn means the host is never Ready at the first open_vault).
                if let Some(vault) = handle.current_vault() {
                    reindex_vault(&handle, &vault);
                }
            }
            PollAction::GiveUp | PollAction::Exited => {
                handle.set_status(HostStatus::Unavailable);
                return;
            }
            PollAction::KeepPolling => {}
        }
        if !ready && !announced && start.elapsed() >= FAST_PATH {
            announced = true;
            log::warn!(
                "[host] still starting (likely first-run model download); will keep polling up to 15m"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Shutdown ladder
// ---------------------------------------------------------------------------

/// How long the host is given to exit on its own after being asked to, before it is stopped.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

pub fn shutdown_host(handle: &HostHandle) {
    // Invalidate any in-flight poll thread before we proceed; this ensures that a
    // poll thread whose /health response arrives after we return cannot overwrite
    // the Unavailable status we set at the end of this function.
    handle.generation.fetch_add(1, Ordering::SeqCst);
    if let Some(host) = handle.endpoint() {
        let _ = host.within(Duration::from_millis(800)).post("/shutdown");
    }
    let mut guard = handle.child.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = guard.take() {
        // Give the graceful shutdown a brief moment, then stop the host and everything it started
        // — even when the host itself has exited, what it started may not have.
        let _ = child.shutdown(SHUTDOWN_GRACE);
    }
    handle.set_status(HostStatus::Unavailable);
}

// ---------------------------------------------------------------------------
// Host executable resolution (mechanism B — parallel to publish::canopy_from_resource_dir)
// ---------------------------------------------------------------------------

/// Resolves the bundled host exe from a Tauri resource directory: `<resource>/host/textree-host(.exe)`.
/// Returns the absolute path, or `None` if it is missing (e.g. an unbundled dev build).
pub fn host_exe_from_resource_dir(resource: &Path) -> Option<String> {
    let name = if cfg!(windows) {
        "textree-host.exe"
    } else {
        "textree-host"
    };
    let exe = resource.join("host").join(name);
    if exe.exists() {
        Some(exe.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// How to launch the host. Dev/E2E builds: `TEXTREE_HOST_EXE` env (auto-consent — keeps the
/// existing dev/test flow eager-spawning). A shipped build never reads it (`dev_env`): it uses the
/// bundled sidecar under `<resource>/host/`.
pub fn resolve_host(app: &AppHandle) -> Option<String> {
    if let Some(exe) = crate::dev_env::var("TEXTREE_HOST_EXE") {
        return Some(exe);
    }
    let resource = app.path().resource_dir().ok()?;
    host_exe_from_resource_dir(&resource)
}

// ---------------------------------------------------------------------------
// DTOs, response parser, and IPC commands
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SemanticHit {
    pub path: String,
    pub snippet: String,
    pub score: f32,
}

#[derive(Deserialize)]
struct SearchResponseDto {
    results: Vec<SemanticHit>,
    #[allow(dead_code)]
    status: String,
}

pub fn parse_search_response(body: &str) -> Option<Vec<SemanticHit>> {
    serde_json::from_str::<SearchResponseDto>(body)
        .ok()
        .map(|r| r.results)
}

/// Payload returned by the `host_status` IPC command.
/// `rename_all = "camelCase"` is load-bearing: the frontend expects `generatorReady` (camelCase).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatusPayload {
    pub status: HostStatus,
    pub generator_ready: bool,
    pub generator_error: Option<String>,
    pub embedder_error: Option<String>,
    pub embedder_download: Option<DownloadSnapshot>,
    pub generator_download: Option<DownloadSnapshot>,
    pub active_provider: String,
}

#[tauri::command]
pub fn host_status(host: State<'_, Arc<HostHandle>>) -> HostStatusPayload {
    HostStatusPayload {
        status: host.status(),
        generator_ready: host.generator_ready(),
        generator_error: host.generator_error(),
        embedder_error: host.embedder_error(),
        embedder_download: host.embedder_download(),
        generator_download: host.generator_download(),
        active_provider: host.active_provider(),
    }
}

/// Resolve which API key `test_byo_connection` should send: prefer the caller-supplied key
/// (fresh input the user just typed, not yet saved to the keychain) and only fall back to
/// `lookup` (the stored key) when none was given. This is the same fallback `prepare_ai_model`/
/// `restart_ai_host` apply via `byo_secret::get_api_key()`, adapted for a command that (unlike
/// those two) takes an explicit `api_key` param — without this, reopening Settings after a
/// previous Save leaves the input blank and Test Connection would send no `Authorization`
/// header at all, producing a false 401 against a server that requires auth even though Save
/// would work fine. `lookup` is injected so the fallback branch is unit-testable without a
/// real network call.
fn resolve_test_key(
    api_key: Option<String>,
    lookup: impl FnOnce() -> Option<String>,
) -> Option<String> {
    api_key.filter(|k| !k.is_empty()).or_else(lookup)
}

/// What a successful connection check found.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ByoConnection {
    pub model_count: u32,
}

/// The JSON line `textree-host probe-byo` prints (`ByoProbeCommand.Outcome` on the .NET side).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeOutcome {
    ok: bool,
    model_count: u32,
    status: Option<u16>,
    error: Option<String>,
    /// Why it failed, as a kind (`ByoProbeFailure`, camel-cased) — `error` is the system's or the
    /// provider's own text, which may be in another language, so the words shown are chosen from this.
    #[serde(default)]
    failure: Option<String>,
}

/// The first argument that makes the host check the BYO settings and exit instead of serving.
const PROBE_BYO_COMMAND: &str = "probe-byo";

/// How long a check may take: the host's own wait for the server (10 s) plus its start-up.
const PROBE_DEADLINE: Duration = Duration::from_secs(25);

/// Settings ▸ Advanced "Test connection". Runs the host once with the settings being tested —
/// `textree-host probe-byo` — which asks the server for its model list through the same preset
/// catalogue the host serves from, so a wrong key, address or server comes back as a failure for
/// every preset. Does NOT touch the running host: the user gets an answer before committing to a
/// host restart.
///
/// Off the main thread: a server that does not answer holds the check for the whole deadline, and
/// on the main thread that is the whole window.
#[tauri::command]
pub async fn test_byo_connection(
    app: AppHandle,
    preset: String,
    base_url: String,
    api_key: Option<String>,
) -> Result<ByoConnection, String> {
    let exe = resolve_host(&app).ok_or_else(|| {
        "The AI helper isn't installed, so the connection can't be checked.".to_string()
    })?;
    crate::commands::off_main(move || {
        let key = resolve_test_key(api_key, crate::byo_secret::get_api_key);
        probe_byo_connection(&exe, &preset, &base_url, key.as_deref())
    })
    .await
}

/// The check behind [`test_byo_connection`]: one run of the host's `probe-byo` command. The key
/// travels in the environment, never on the command line.
pub fn probe_byo_connection(
    exe: &str,
    preset: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<ByoConnection, String> {
    let mut cmd = Command::new(exe);
    cmd.arg(PROBE_BYO_COMMAND)
        .env_remove("TEXTREE_BYO_API_KEY")
        .env("TEXTREE_BYO_PRESET", preset)
        .env("TEXTREE_BYO_BASE_URL", base_url);
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        cmd.env("TEXTREE_BYO_API_KEY", key);
    }
    let mut child = Sidecar::spawn(
        cmd,
        Output::Lines {
            stdout: None,
            stderr: Sink::Discard,
        },
    )
    .map_err(|e| format!("The AI helper could not be started: {e}"))?;
    let line = child.wait_line(PROBE_DEADLINE, |l| l.trim_start().starts_with('{'));
    let _ = child.shutdown(Duration::ZERO);
    match line.map_err(|e| e.to_string())? {
        LineReadiness::Line(l) => read_probe_outcome(&l),
        LineReadiness::Exited(_) => {
            Err("The AI helper stopped before it could check the connection.".to_string())
        }
        LineReadiness::TimedOut => Err("The server didn't answer in time.".to_string()),
    }
}

/// Turns the host's answer into what Settings shows. Pure, so the wording is unit-tested.
fn read_probe_outcome(line: &str) -> Result<ByoConnection, String> {
    let o: ProbeOutcome = serde_json::from_str(line)
        .map_err(|_| "The AI helper gave an answer this version doesn't understand.".to_string())?;
    if o.ok {
        return Ok(ByoConnection {
            model_count: o.model_count,
        });
    }
    Err(match (o.status, o.failure.as_deref()) {
        (Some(401) | Some(403), _) | (None, Some("unauthorized")) => "The server didn't accept the API key.".to_string(),
        (Some(404), _) => "Nothing answers at this address — check the server URL.".to_string(),
        (Some(code), _) => match o.error {
            Some(e) if !e.is_empty() => format!("The server refused the request ({code}): {e}"),
            _ => format!("The server refused the request ({code})."),
        },
        // No status: nothing answered. The kind says why; the helper's own text may be in the
        // system's language, so it is not shown.
        (None, Some("refused")) => {
            "Nothing is listening at this address — check the port and that the server is running.".to_string()
        }
        (None, Some("hostNotFound")) => "No server goes by that name — check the address.".to_string(),
        (None, Some("timeout")) => {
            "The server didn't answer in time — check the address and that the server is running.".to_string()
        }
        (None, Some("invalid")) => "That isn't a server address — check what you entered.".to_string(),
        (None, Some("transport")) => {
            "The connection to the server broke off — check whether the address starts with http or https.".to_string()
        }
        _ => "The server couldn't be reached — check the address and that the server is running.".to_string(),
    })
}

/// Fire-and-forget: bump ask_generation so any in-flight `ask` stream detects the mismatch,
/// drops its reader, and closes the TCP connection — causing the host to see RequestAborted
/// and stop generating. Called by the frontend on panel close / note switch.
/// No params; no return value needed — bumping the counter is the entire effect.
#[tauri::command]
pub fn cancel_ask(host: State<'_, Arc<HostHandle>>) {
    host.bump_ask_generation();
}

/// Fire-and-forget: ask the host to warm its generator (model loading, KV cache pre-fill, etc.).
/// Called by the frontend when the user opens the Ask panel so the first token arrives faster.
/// Graceful: no-op when the host is not up or not Ready.
#[tauri::command]
pub async fn prepare_generation(host: State<'_, Arc<HostHandle>>) -> Result<(), String> {
    let Some(endpoint) = host.endpoint() else {
        return Ok(());
    };
    if !matches!(host.status(), HostStatus::Ready) {
        return Ok(());
    }
    // A (re)prepare attempt clears the last generator error optimistically: otherwise a stale
    // error from before this attempt lingers in host_status until the next health poll (up to
    // READY_POLL_INTERVAL), bouncing the frontend retry loop straight back to 'error'.
    host.set_generator_error(None);
    tauri::async_runtime::spawn_blocking(move || {
        let _ = endpoint
            .within(Duration::from_secs(10))
            .post("/prepare-generation");
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// User-triggered: spawn the local-AI host now. Called by the frontend after the user consents
/// (or at mount when the device-local consent flag is set). No-op if already up; graceful (Ok with
/// no spawn) when no host exe is bundled/available — AI simply stays off.
#[tauri::command]
pub fn prepare_ai_model(
    app: AppHandle,
    host: State<'_, Arc<HostHandle>>,
    preset: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    if matches!(host.status(), HostStatus::Starting | HostStatus::Ready) {
        return Ok(());
    }
    let Some(exe) = resolve_host(&app) else {
        return Ok(()); // no host available → graceful degradation
    };
    let log_dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let api_key = if preset.is_some() {
        crate::byo_secret::get_api_key()
    } else {
        None
    };
    let active_provider = preset.clone().unwrap_or_else(|| "local".to_string());
    let extra_env = resolve_byo_env(preset, base_url, model, api_key);
    spawn_host(
        host.inner().clone(),
        exe,
        log_dir,
        extra_env,
        active_provider,
    );
    Ok(())
}

/// Force-reconfigure: unlike `prepare_ai_model` (idempotent no-op if already up), this always
/// stops the current host and spawns a fresh one — needed when Settings ▸Advanced changes the
/// BYO config for a host that's already running (prepare_ai_model's "already up -> no-op" guard
/// would otherwise strand the old config in place). `preset=None` switches back to the bundled
/// local model.
#[tauri::command]
pub fn restart_ai_host(
    app: AppHandle,
    host: State<'_, Arc<HostHandle>>,
    preset: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    shutdown_host(&host);
    let Some(exe) = resolve_host(&app) else {
        return Ok(());
    };
    let log_dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let api_key = if preset.is_some() {
        crate::byo_secret::get_api_key()
    } else {
        None
    };
    let active_provider = preset.clone().unwrap_or_else(|| "local".to_string());
    let extra_env = resolve_byo_env(preset, base_url, model, api_key);
    spawn_host(
        host.inner().clone(),
        exe,
        log_dir,
        extra_env,
        active_provider,
    );
    Ok(())
}

/// User-triggered (Settings → turn local AI off): stop the host now and free its process.
/// Thin wrapper over `shutdown_host` (no new logic). No-op-safe when the host is already down.
/// The on-disk index is a regenerable cache and is left intact: nothing a person wrote is altered.
#[tauri::command]
pub fn stop_host(host: State<'_, Arc<HostHandle>>) -> Result<(), String> {
    shutdown_host(&host);
    Ok(())
}

/// Where a scope sent relative to the vault is: a folder or note inside it, or the whole vault
/// when there is none (`""` alike). The host is handed the absolute path it indexes by.
fn scope_within(vault: &str, scope: Option<String>) -> Result<String, String> {
    crate::vault_paths::folder(vault, scope.as_deref().unwrap_or(""))
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn semantic_search(
    root_id: String,
    query: String,
    scope: Option<String>,
    limit: u32,
    host: State<'_, Arc<HostHandle>>,
) -> Result<Vec<SemanticHit>, String> {
    let vault = crate::workspace::path_of(&root_id)?;
    // Read cheap mutex fields before entering the blocking closure.
    let Some(endpoint) = host.endpoint() else {
        return Ok(Vec::new()); // host not up → empty, caller falls back / shows unavailable
    };
    if !matches!(host.status(), HostStatus::Ready) {
        return Ok(Vec::new());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let scope = scope_within(&vault, scope)?;
        let payload = serde_json::json!({
            "vaultPath": vault,
            "query": query,
            "scopePath": scope,
            "limit": limit,
        });
        match endpoint
            .within(Duration::from_secs(10))
            .post_json("/search", &payload.to_string())
        {
            Ok(resp) if resp.is_success() => {
                Ok(parse_search_response(&resp.body).unwrap_or_default())
            }
            Ok(resp) => Err(format!("http status: {}", resp.status)),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// DTOs and helpers for `ask` (streaming /chat)
// ---------------------------------------------------------------------------

/// A single message in a conversation, forwarded from the frontend to the host's /chat endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Events emitted by the `ask` command over its `Channel<AskEvent>`.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AskEvent {
    /// A streamed text token from the model.
    Token { text: String },
    /// The semantic-search hits that were used to ground the answer (sent once, after Done).
    Citations { hits: Vec<SemanticHit> },
    /// Stream complete — no more events will follow on this channel. `reason`: why the answer
    /// ended, when it ended short of finishing — absent when it finished or nobody said.
    Done {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<AnswerEnd>,
    },
    /// A non-fatal error; the frontend should display the message to the user.
    Error { message: String },
}

// Private SSE-parsing DTOs.
#[derive(Deserialize)]
struct ChatChunk {
    choices: Vec<ChatChoice>,
}
#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: Option<ChatDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Why an answer ended short of finishing. Only the reasons the person is told about: an answer
/// that finished ("stop") or whose end nobody explained is told nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerEnd {
    /// It reached the length limit.
    Length,
    /// It fell into repeating itself and was stopped.
    Degeneration,
}

impl AnswerEnd {
    fn from_reason(reason: &str) -> Option<Self> {
        match reason {
            "length" => Some(Self::Length),
            "degeneration" => Some(Self::Degeneration),
            _ => None,
        }
    }
}

/// What one SSE line of an answer carries.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChatLine {
    /// A piece of the answer.
    pub text: Option<String>,
    /// Why the answer ended, on the last line before `[DONE]`.
    pub end: Option<AnswerEnd>,
}
#[derive(Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: String,
}

/// Parse one SSE line (`"data: {...}"`): a piece of the answer, why it ended, or both.
///
/// Returns `None` for:
/// - `data: [DONE]` (stream terminator)
/// - blank lines (SSE field separator)
/// - non-`data:` lines (event/id/comment fields)
/// - JSON parse failures (malformed chunk)
/// - a line with neither text nor a reason told to the person (role-only, heartbeat, "stop")
pub fn parse_chat_line(line: &str) -> Option<ChatLine> {
    let payload = line.strip_prefix("data: ")?;
    if payload.trim() == "[DONE]" {
        return None;
    }
    let chunk: ChatChunk = serde_json::from_str(payload).ok()?;
    let choice = chunk.choices.into_iter().next()?;
    let parsed = ChatLine {
        text: choice.delta.map(|d| d.content).filter(|t| !t.is_empty()),
        end: choice
            .finish_reason
            .as_deref()
            .and_then(AnswerEnd::from_reason),
    };
    (parsed != ChatLine::default()).then_some(parsed)
}

/// Stream a conversational response from the local-AI host's `/chat` endpoint.
///
/// Events are pushed over `on_event`:
/// 1. `Token { text }` — one per SSE delta, in order.
/// 2. `Citations { hits }` — the pre-fetched semantic hits used to ground the answer.
/// 3. `Done { reason }` — stream complete, with why it ended short when the host said.
///
/// On any error (host not up, connection failure, I/O) an `Error { message }` event is sent
/// instead of Token/Citations/Done, and the command returns `Ok(())` so the frontend does
/// not receive a Tauri IPC error.
///
/// Single-flight cancellation: a new `ask` call bumps `ask_generation`; in-flight streams
/// detect the mismatch on each line and abort (dropping the reader closes the TCP connection,
/// causing the host to see `RequestAborted` and stop generating — freeing CPU/memory).
///
/// Scope validation mirrors `semantic_search`: `scope`, when provided, must be within the vault,
/// or the command returns `Err` (an IPC-level error, not an event).
#[tauri::command]
pub async fn ask(
    root_id: String,
    messages: Vec<ChatMessage>,
    citation_hits: Vec<SemanticHit>,
    scope: Option<String>,
    on_event: tauri::ipc::Channel<AskEvent>,
    host: State<'_, Arc<HostHandle>>,
) -> Result<(), String> {
    let vault = crate::workspace::path_of(&root_id)?;
    // Graceful degradation: host not up → inform the user, do not hard-error.
    let Some(endpoint) = host.endpoint() else {
        let _ = on_event.send(AskEvent::Error {
            message: "Local AI is not available".into(),
        });
        return Ok(());
    };
    if !matches!(host.status(), HostStatus::Ready) {
        let _ = on_event.send(AskEvent::Error {
            message: "Local AI is preparing".into(),
        });
        return Ok(());
    }
    // Security at the edge: a scope outside the vault is refused, as for `semantic_search`.
    {
        let vault = vault.clone();
        crate::commands::off_main(move || scope_within(&vault, scope).map(drop)).await?;
    }

    // Single-flight cancellation counter (separate from health-poll `generation`).
    let my_gen = host.bump_ask_generation();
    let handle = host.inner().clone();

    // Clone the channel so on_event is available both inside and after spawn_blocking.
    // Channel<T> is Clone in Tauri 2; both halves push to the same frontend listener.
    let on_event_inner = on_event.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        let payload = serde_json::json!({ "messages": messages });
        let reader = endpoint
            .within(Duration::from_secs(120))
            .post_json_stream("/chat", &payload.to_string())
            .map_err(|e| e.to_string())?;
        if !reader.is_success() {
            return Err(format!("http status: {}", reader.status));
        }
        let mut end = None;
        for line in reader.lines() {
            // A newer `ask` has started — abandon this stream.
            // Dropping `reader` closes the TCP connection; the host sees RequestAborted
            // and stops generating, freeing its CPU/memory budget.
            if handle.ask_generation() != my_gen {
                return Ok(None);
            }
            let line = line.map_err(|e| e.to_string())?;
            let Some(parsed) = parse_chat_line(&line) else {
                continue;
            };
            if let Some(text) = parsed.text {
                let _ = on_event_inner.send(AskEvent::Token { text });
            }
            end = parsed.end.or(end);
        }
        Ok::<Option<AnswerEnd>, String>(end)
    })
    .await
    .map_err(|e| e.to_string())?;

    match result {
        Ok(reason) => {
            // Guard against sending Citations/Done on a superseded channel (cancelled ask).
            // If another ask has since started, our channel is stale — skip the epilogue.
            if host.ask_generation() == my_gen {
                let _ = on_event.send(AskEvent::Citations {
                    hits: citation_hits,
                });
                let _ = on_event.send(AskEvent::Done { reason });
            }
        }
        Err(e) => {
            let _ = on_event.send(AskEvent::Error { message: e });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fire-and-forget indexing helpers
// ---------------------------------------------------------------------------

/// Fire-and-forget single-note index. Silent on any error (degrade, don't block).
/// Runs without asking because it only reads: nothing a person wrote is altered.
///
/// Only for a note of the folder open now. A save can finish after the person has opened another
/// folder; the host keeps one folder's index active at a time, and a request for another folder
/// makes it switch — dropping the one in use, a reindex of it included. The note reaches its own
/// folder's index when that folder is opened again and reindexed.
pub fn index_note(handle: &HostHandle, vault: &str, path: &str) {
    if !handle.is_current_vault(vault) {
        log::debug!("[host] {path} is a note of a folder that is not open; not indexed");
        return;
    }
    let Some(host) = handle.endpoint() else {
        return;
    };
    if !matches!(handle.status(), HostStatus::Ready) {
        return;
    }
    let payload = serde_json::json!({ "vaultPath": vault, "path": path });
    let _ = host
        .within(Duration::from_secs(10))
        .post_json("/index", &payload.to_string());
}

/// Fire-and-forget full vault reindex. Silent on any error (degrade, don't block).
/// Runs without asking because it only reads: nothing a person wrote is altered.
pub fn reindex_vault(handle: &HostHandle, vault: &str) {
    let Some(host) = handle.endpoint() else {
        return;
    };
    if !matches!(handle.status(), HostStatus::Ready) {
        return;
    }
    let payload = serde_json::json!({ "vaultPath": vault });
    let _ = host
        .within(Duration::from_secs(10))
        .post_json("/reindex", &payload.to_string());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_is_sent_relative_to_the_vault_and_kept_inside_it() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("projects/2026")).unwrap();
        let vault = d.path().to_string_lossy().into_owned();
        assert_eq!(scope_within(&vault, None).unwrap(), vault);
        assert_eq!(scope_within(&vault, Some(String::new())).unwrap(), vault);
        let inner = scope_within(&vault, Some("projects/2026".into())).unwrap();
        assert_eq!(Path::new(&inner), d.path().join("projects").join("2026"));
        assert_eq!(
            scope_within(&vault, Some("../elsewhere".into())).unwrap_err(),
            "path is outside the vault"
        );
    }

    /// A client for a port nothing listens on.
    fn unreachable_endpoint() -> Client {
        let token = tauri_kit_sidecar::loopback::Token::generate().unwrap();
        Client::new(
            &tauri_kit_sidecar::loopback::Endpoint::new(9, token),
            &tauri_kit_sidecar::loopback::ClientOptions::default(),
        )
    }

    #[test]
    fn parse_health_reads_embedder_ready() {
        let h = parse_health(r#"{"status":"ok","embedderReady":true}"#).unwrap();
        assert_eq!(h.status, "ok");
        assert!(h.embedder_ready);
    }

    #[test]
    fn parse_health_handles_not_ready() {
        let h = parse_health(r#"{"status":"loading","embedderReady":false}"#).unwrap();
        assert!(!h.embedder_ready);
    }

    #[test]
    fn parse_health_rejects_garbage() {
        assert!(parse_health("not json").is_none());
    }

    #[test]
    fn parse_search_response_maps_hits() {
        let body = r#"{"results":[{"path":"a.md","snippet":"hello","score":0.9}],"status":"ok"}"#;
        let hits = parse_search_response(body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.md");
        assert!((hits[0].score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn parse_search_response_empty_on_no_results() {
        let hits = parse_search_response(r#"{"results":[],"status":"ok"}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn host_exe_from_resource_dir_finds_bundled_exe() {
        let tmp = std::env::temp_dir().join(format!("textree-host-resolve-{}", std::process::id()));
        let host_dir = tmp.join("host");
        std::fs::create_dir_all(&host_dir).unwrap();
        let name = if cfg!(windows) {
            "textree-host.exe"
        } else {
            "textree-host"
        };
        let exe = host_dir.join(name);
        std::fs::write(&exe, b"stub").unwrap();

        let found = host_exe_from_resource_dir(&tmp);
        assert_eq!(found.as_deref(), exe.to_str());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn host_exe_from_resource_dir_none_when_absent() {
        let tmp = std::env::temp_dir().join(format!("textree-host-absent-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(host_exe_from_resource_dir(&tmp).is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn spawn_host_is_noop_when_already_up() {
        let handle = Arc::new(HostHandle::default());
        handle.set_status(HostStatus::Ready); // pretend the host is already running
        let tmp = std::env::temp_dir().join("textree-host-guard");
        // A bogus exe would fail to spawn and flip status to Unavailable WITHOUT the guard.
        spawn_host(
            handle.clone(),
            "this-exe-does-not-exist".into(),
            tmp,
            Vec::new(),
            "local".to_string(),
        );
        assert!(
            matches!(handle.status(), HostStatus::Ready),
            "guard must prevent re-spawn when already Starting/Ready"
        );
    }

    #[test]
    fn try_begin_spawn_claims_then_rejects_second_caller() {
        let handle = HostHandle::default(); // status = Unavailable
        assert!(
            handle.try_begin_spawn(),
            "first claim from a non-running host must win"
        );
        assert!(
            !handle.try_begin_spawn(),
            "second claim must lose — a spawn is already in flight (Starting)"
        );
    }

    #[test]
    fn try_begin_spawn_rejected_when_already_ready() {
        let handle = HostHandle::default();
        handle.set_status(HostStatus::Ready);
        assert!(
            !handle.try_begin_spawn(),
            "claim must be rejected when the host is already Ready"
        );
    }

    #[test]
    fn try_begin_spawn_admits_exactly_one_concurrent_winner() {
        // The race the guard exists to prevent: many callers hitting an idle host at once.
        // Exactly one may claim; the rest must be turned away so only one child is spawned.
        let handle = Arc::new(HostHandle::default());
        let winners: usize = (0..16)
            .map(|_| {
                let h = handle.clone();
                std::thread::spawn(move || h.try_begin_spawn())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|j| j.join().unwrap() as usize)
            .sum();
        assert_eq!(
            winners, 1,
            "exactly one concurrent caller may claim the spawn"
        );
    }

    #[test]
    fn current_vault_round_trips() {
        let handle = HostHandle::default();
        assert!(handle.current_vault().is_none());
        handle.set_current_vault("D:/vault".into());
        assert_eq!(handle.current_vault().as_deref(), Some("D:/vault"));
    }

    #[test]
    fn only_the_current_vault_is_indexed() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        let (a_str, b_str) = (a.path().to_string_lossy(), b.path().to_string_lossy());
        let handle = HostHandle::default();
        assert!(!handle.is_current_vault(&a_str), "no vault is open yet");

        handle.set_current_vault(b_str.to_string());
        assert!(
            !handle.is_current_vault(&a_str),
            "a note of the folder open before is not indexed"
        );
        assert!(handle.is_current_vault(&b_str));
        // The same folder spelled another way is still the same folder.
        let detour = b.path().join("sub").join("..");
        std::fs::create_dir(b.path().join("sub")).unwrap();
        assert!(handle.is_current_vault(&detour.to_string_lossy()));
    }

    #[test]
    #[ignore = "requires assemble-host-sidecar.ps1 to have staged the exe under src-tauri/resources/host/"]
    fn run_host_present_via_assembled_sidecar() {
        // CARGO_MANIFEST_DIR = src-tauri/. The assembled payload lives under resources/host/.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let resource = manifest.join("resources");
        let found = host_exe_from_resource_dir(&resource);
        assert!(
            found.is_some(),
            "assembled host exe not found under {}/host/ — run scripts/assemble-host-sidecar.ps1",
            resource.display()
        );
    }

    #[test]
    fn poll_action_stops_when_the_process_has_exited() {
        // Whatever /health would have said: during startup (a crash no longer waits out the
        // 15-minute ceiling), after Ready (a dead host is no longer reported as Ready), and even
        // with the embedder reported ready by a response that raced the exit.
        for embedder_ready in [false, true] {
            for already_ready in [false, true] {
                for ceiling_exceeded in [false, true] {
                    assert_eq!(
                        poll_action(true, embedder_ready, already_ready, ceiling_exceeded),
                        PollAction::Exited
                    );
                }
            }
        }
    }

    #[test]
    fn a_host_that_crashes_during_startup_is_reported_unavailable_within_seconds() {
        // Before, the poll loop only asked /health: a host that died at startup read as "still
        // starting" until the 15-minute ceiling.
        let handle = Arc::new(HostHandle::default());
        handle.set_status(HostStatus::Starting);
        let cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "exit 7"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 7"]);
            c
        };
        *handle.child.lock().unwrap() =
            Some(Sidecar::spawn(cmd, Output::Discard).expect("spawn a process that exits"));

        let started = Instant::now();
        let poller = {
            let handle = handle.clone();
            std::thread::spawn(move || poll_health(handle, unreachable_endpoint(), 0))
        };
        poller.join().unwrap();
        assert!(matches!(handle.status(), HostStatus::Unavailable));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn poll_action_becomes_ready_on_embedder_ready() {
        assert_eq!(
            poll_action(false, true, false, false),
            PollAction::BecomeReady
        );
    }

    #[test]
    fn poll_action_embedder_ready_beats_ceiling() {
        // Embedder up right at the ceiling still transitions to Ready (not GiveUp).
        assert_eq!(
            poll_action(false, true, false, true),
            PollAction::BecomeReady
        );
    }

    #[test]
    fn poll_action_keeps_polling_while_starting_within_ceiling() {
        assert_eq!(
            poll_action(false, false, false, false),
            PollAction::KeepPolling
        );
    }

    #[test]
    fn poll_action_gives_up_when_never_ready_past_ceiling() {
        assert_eq!(poll_action(false, false, false, true), PollAction::GiveUp);
    }

    #[test]
    fn poll_action_keeps_polling_forever_once_ready() {
        // Once Ready we keep refreshing generatorReady for the host's lifetime regardless of
        // embedder blips or the startup ceiling — never GiveUp (that would mark a healthy host
        // dead). This is the invariant that fixes the frozen-generatorReady bug: the loop must
        // keep observing /health after embedder_ready so the lazily-loaded generator is seen.
        assert_eq!(
            poll_action(false, true, true, true),
            PollAction::KeepPolling
        );
        assert_eq!(
            poll_action(false, false, true, true),
            PollAction::KeepPolling
        );
        assert_eq!(
            poll_action(false, false, true, false),
            PollAction::KeepPolling
        );
    }

    #[test]
    fn parse_health_reads_download_snapshot() {
        let body = r#"{"status":"ok","embedderReady":false,"embedderError":null,
          "embedderDownload":{"phase":"downloading","overallPercent":42.0,
          "bytesDownloaded":1200000000,"totalBytes":2900000000,"fileIndex":2,"fileCount":3}}"#;
        let h = parse_health(body).unwrap();
        assert!(!h.embedder_ready);
        let dl = h.embedder_download.unwrap();
        assert_eq!(dl.file_count, 3);
        assert_eq!(dl.total_bytes, 2_900_000_000);
        assert!((dl.overall_percent - 42.0).abs() < 0.01);
    }

    #[test]
    fn parse_health_tolerates_missing_download_fields() {
        let body = r#"{"status":"ok","embedderReady":true,"generatorReady":true}"#;
        let h = parse_health(body).unwrap();
        assert!(h.embedder_download.is_none());
        assert!(h.generator_download.is_none());
    }

    #[test]
    fn parse_health_reads_generator_ready() {
        let body = r#"{"status":"ok","embedderReady":true,"generatorReady":false}"#;
        let h = parse_health(body).unwrap();
        assert!(h.embedder_ready);
        assert!(!h.generator_ready);
    }

    #[test]
    fn parse_health_reads_generator_error() {
        let body = r#"{"status":"ok","embedderReady":true,"generatorReady":false,"generatorError":"model download failed"}"#;
        let h = parse_health(body).unwrap();
        assert_eq!(h.generator_error.as_deref(), Some("model download failed"));
    }

    #[test]
    fn parse_health_generator_error_defaults_none() {
        let body = r#"{"status":"ok","embedderReady":true,"generatorReady":false}"#;
        let h = parse_health(body).unwrap();
        assert!(h.generator_error.is_none());
    }

    #[test]
    fn poll_interval_fast_during_any_download_even_when_ready() {
        // generator downloading after embedder ready → must stay fast (1s), not back off
        assert_eq!(poll_interval(true, true), POLL_INTERVAL);
    }

    #[test]
    fn poll_interval_backs_off_only_when_ready_and_idle() {
        assert_eq!(poll_interval(true, false), READY_POLL_INTERVAL);
    }

    #[test]
    fn poll_interval_fast_while_starting() {
        assert_eq!(poll_interval(false, false), POLL_INTERVAL);
    }

    #[test]
    fn generator_error_round_trips() {
        let handle = HostHandle::default();
        assert!(handle.generator_error().is_none());
        handle.set_generator_error(Some("boom".into()));
        assert_eq!(handle.generator_error().as_deref(), Some("boom"));
        handle.set_generator_error(None);
        assert!(handle.generator_error().is_none());
    }

    #[test]
    fn parse_chat_line_extracts_delta_content() {
        // Valid delta with text.
        let line = r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#;
        assert_eq!(
            parse_chat_line(line),
            Some(ChatLine {
                text: Some("Hel".into()),
                end: None
            })
        );
        // Stream terminator — must return None.
        assert_eq!(parse_chat_line("data: [DONE]"), None);
        // Blank line (SSE separator) — must return None.
        assert_eq!(parse_chat_line(""), None);
        // Empty content field — model emitted an empty delta, not a real token.
        let empty_delta = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
        assert_eq!(parse_chat_line(empty_delta), None);
    }

    #[test]
    fn parse_chat_line_reads_why_the_answer_ended() {
        let length = r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#;
        assert_eq!(
            parse_chat_line(length),
            Some(ChatLine {
                text: None,
                end: Some(AnswerEnd::Length)
            })
        );
        let repeating = r#"data: {"choices":[{"delta":{},"finish_reason":"degeneration"}]}"#;
        assert_eq!(
            parse_chat_line(repeating).and_then(|l| l.end),
            Some(AnswerEnd::Degeneration)
        );
        // An answer that finished, or a reason nobody explains to the person, says nothing.
        assert_eq!(
            parse_chat_line(r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            None
        );
        assert_eq!(
            parse_chat_line(r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
            None
        );
        // Some servers put the reason on the last text chunk, or send no delta at all.
        let both = r#"data: {"choices":[{"delta":{"content":"end"},"finish_reason":"length"}]}"#;
        assert_eq!(
            parse_chat_line(both),
            Some(ChatLine {
                text: Some("end".into()),
                end: Some(AnswerEnd::Length)
            })
        );
        let no_delta = r#"data: {"choices":[{"finish_reason":"length"}]}"#;
        assert_eq!(
            parse_chat_line(no_delta).and_then(|l| l.end),
            Some(AnswerEnd::Length)
        );
    }

    #[test]
    fn done_carries_the_reason_only_when_there_is_one() {
        assert_eq!(
            serde_json::to_value(AskEvent::Done {
                reason: Some(AnswerEnd::Length)
            })
            .unwrap(),
            serde_json::json!({ "kind": "done", "reason": "length" })
        );
        assert_eq!(
            serde_json::to_value(AskEvent::Done { reason: None }).unwrap(),
            serde_json::json!({ "kind": "done" })
        );
    }

    #[test]
    fn resolve_byo_env_empty_when_no_preset() {
        let env = resolve_byo_env(None, None, None, None);
        assert!(env.is_empty());
    }

    #[test]
    fn resolve_byo_env_includes_preset_baseurl_model_when_preset_present() {
        let env = resolve_byo_env(
            Some("ollama".to_string()),
            Some("http://localhost:11434".to_string()),
            Some("llama3".to_string()),
            None,
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TEXTREE_BYO_PRESET")
                .map(|(_, v)| v.as_str()),
            Some("ollama")
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TEXTREE_BYO_BASE_URL")
                .map(|(_, v)| v.as_str()),
            Some("http://localhost:11434")
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TEXTREE_BYO_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("llama3")
        );
        assert!(env.iter().all(|(k, _)| k != "TEXTREE_BYO_API_KEY")); // no key passed in this call
    }

    #[test]
    fn resolve_byo_env_includes_api_key_when_present() {
        let env = resolve_byo_env(
            Some("gpustack".to_string()),
            Some("http://localhost:8080".to_string()),
            None,
            Some("secret".to_string()),
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TEXTREE_BYO_API_KEY")
                .map(|(_, v)| v.as_str()),
            Some("secret")
        );
    }

    #[test]
    fn resolve_test_key_prefers_explicit_key_over_lookup() {
        let key = resolve_test_key(Some("explicit".to_string()), || Some("stored".to_string()));
        assert_eq!(key.as_deref(), Some("explicit"));
    }

    #[test]
    fn resolve_test_key_treats_empty_string_as_absent_and_falls_back() {
        let key = resolve_test_key(Some(String::new()), || Some("stored".to_string()));
        assert_eq!(key.as_deref(), Some("stored"));
    }

    #[test]
    fn resolve_test_key_none_with_no_stored_key_yields_none() {
        let key = resolve_test_key(None, || None);
        assert!(key.is_none());
    }

    #[test]
    fn resolve_test_key_falls_back_to_the_real_stored_keychain_entry() {
        // Exercises the actual fallback path test_byo_connection relies on: a key saved via
        // Settings (byo_secret::set_api_key) must be picked up when the Test Connection input
        // is blank (None), the same way it would be for a freshly reopened Settings panel.
        // Shares byo_secret's TEST_LOCK so this doesn't race its own keychain tests.
        let _guard = crate::byo_secret::tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ = crate::byo_secret::clear_api_key(); // clean slate in case a prior run aborted
        crate::byo_secret::set_api_key("stored-secret-xyz").expect("set should succeed");

        let key = resolve_test_key(None, crate::byo_secret::get_api_key);
        assert_eq!(key.as_deref(), Some("stored-secret-xyz"));

        crate::byo_secret::clear_api_key().expect("cleanup should succeed");
    }

    #[test]
    fn a_reachable_server_reports_its_model_count() {
        let r = read_probe_outcome(r#"{"ok":true,"modelCount":141,"status":null,"error":null}"#);
        assert_eq!(r, Ok(ByoConnection { model_count: 141 }));
    }

    #[test]
    fn a_refused_key_says_so_in_plain_words() {
        let r = read_probe_outcome(
            r#"{"ok":false,"modelCount":0,"status":401,"error":"Unauthorized"}"#,
        );
        assert_eq!(r, Err("The server didn't accept the API key.".to_string()));
        let r =
            read_probe_outcome(r#"{"ok":false,"modelCount":0,"status":404,"error":"Not Found"}"#);
        assert!(r.unwrap_err().contains("check the server URL"));
    }

    #[test]
    fn other_refusals_carry_the_servers_reason_and_silence_says_unreachable() {
        let r = read_probe_outcome(r#"{"ok":false,"modelCount":0,"status":500,"error":"boom"}"#);
        assert_eq!(
            r,
            Err("The server refused the request (500): boom".to_string())
        );
        let r = read_probe_outcome(
            r#"{"ok":false,"modelCount":0,"status":null,"error":"Retry failed after 4 tries."}"#,
        );
        assert!(r.unwrap_err().starts_with("The server couldn't be reached"));
        let r = read_probe_outcome(r#"{"ok":false,"modelCount":0}"#);
        assert!(r.unwrap_err().starts_with("The server couldn't be reached"));
    }

    #[test]
    fn a_connection_failure_is_worded_by_its_kind_not_the_systems_text() {
        let line = |kind: &str| {
            format!(
                r#"{{"ok":false,"modelCount":0,"status":null,"error":"(localized)","failure":"{kind}"}}"#
            )
        };
        assert!(read_probe_outcome(&line("refused"))
            .unwrap_err()
            .starts_with("Nothing is listening"));
        assert!(read_probe_outcome(&line("hostNotFound"))
            .unwrap_err()
            .starts_with("No server goes by that name"));
        assert!(read_probe_outcome(&line("timeout"))
            .unwrap_err()
            .starts_with("The server didn't answer in time"));
        assert!(read_probe_outcome(&line("invalid"))
            .unwrap_err()
            .starts_with("That isn't a server address"));
        assert!(read_probe_outcome(&line("transport"))
            .unwrap_err()
            .starts_with("The connection to the server broke off"));
        assert_eq!(
            read_probe_outcome(&line("unauthorized")),
            Err("The server didn't accept the API key.".to_string())
        );
        for kind in ["unreachable", "somethingNew"] {
            assert!(read_probe_outcome(&line(kind))
                .unwrap_err()
                .starts_with("The server couldn't be reached"));
        }
        assert!(!read_probe_outcome(&line("refused"))
            .unwrap_err()
            .contains("localized"));
    }

    #[test]
    fn an_answer_that_is_not_the_probe_line_is_a_failure_not_a_success() {
        assert!(read_probe_outcome("Unhandled exception").is_err());
    }

    #[test]
    #[ignore = "requires TEXTREE_HOST_EXE pointing at a built host"]
    fn the_host_checks_a_server_that_is_not_there() {
        let exe = std::env::var("TEXTREE_HOST_EXE").expect("set TEXTREE_HOST_EXE");
        let r = probe_byo_connection(&exe, "custom", "http://127.0.0.1:9/v1", None);
        let e = r.expect_err("nothing listens on port 9");
        assert!(e.starts_with("Nothing is listening"), "{e}");
    }

    #[test]
    fn shutdown_host_is_safe_when_no_host_running() {
        // The host-absent path the Settings "turn off" relies on: no base_url, no child.
        // shutdown must not panic and must leave status Unavailable (no network call made).
        let handle = HostHandle::default();
        shutdown_host(&handle);
        assert!(matches!(handle.status(), HostStatus::Unavailable));
    }

    #[test]
    #[ignore = "requires TEXTREE_HOST_EXE pointing at a built host"]
    fn spawn_reaches_ready() {
        let exe = std::env::var("TEXTREE_HOST_EXE").expect("set TEXTREE_HOST_EXE");
        let handle = Arc::new(HostHandle::default());
        let tmp = std::env::temp_dir().join("textree-host-test");
        spawn_host(handle.clone(), exe, tmp, Vec::new(), "local".to_string());
        // poll up to 90s for readiness (model may download)
        for _ in 0..90 {
            if matches!(handle.status(), HostStatus::Ready) {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(matches!(handle.status(), HostStatus::Ready));
        shutdown_host(&handle);
        assert!(
            matches!(handle.status(), HostStatus::Unavailable),
            "status must be Unavailable after shutdown (generation-gate fix)"
        );
    }
}

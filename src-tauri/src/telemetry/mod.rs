//! Content-free telemetry. Nothing leaves the device unless an official build carries a connection
//! string, and what does leave carries no paths, file names, note titles, folder identifiers,
//! search queries or content:
//!
//! - **Failures** — a panic, a default folder that cannot be made — are `tauri-kit-diagnostics`
//!   reports: built from an allowlist only (the kind of failure, our own source frames, the details
//!   `registry.json` declares — an OS error number), appended to a file in the `reports` folder of
//!   the app's log folder — exactly what would be sent, readable by the person — and sent from there
//!   in the background. What could not go out yet stays for the next launch, so a panic is delivered
//!   even though the process it ended cannot send it.
//! - **`app.launch`** — one usage event per start, which is not a failure and so not a report.

pub mod envelope;
pub mod event;

use event::{EnvFacts, LaunchEvent};
use std::path::Path;
use std::sync::{LazyLock, OnceLock};
use tauri_kit_diagnostics::{
    adopt, tidy, FrameRule, Layer, Registry, Report, Reporter, Segment, Sink, MAX_STREAM_BYTES,
};

/// The only switch for outbound telemetry. Absent/blank ⇒ disabled (dev, test, forks, and
/// official builds whose CI has not injected the string yet).
pub const ENV_VAR: &str = "TEXTREE_TELEMETRY_CONNECTION";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a report may carry besides its kind and our own source frames — declared, compiled in.
static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    Registry::from_json(include_str!("registry.json"))
        .expect("telemetry/registry.json is a registry")
});

/// This process, as reports name it — the role the AI helper's own telemetry sits beside.
static SHELL: LazyLock<Layer> =
    LazyLock::new(|| REGISTRY.layer("textree-desktop", FrameRule::rust_source()));

/// Set once the log folder is known (`start`); a failure before that is not recorded.
static REPORTER: OnceLock<Reporter> = OnceLock::new();

/// This process's file in the reports folder, held for as long as the process writes it.
static SEGMENT: OnceLock<Segment> = OnceLock::new();

/// Raw connection string from runtime env first (dev/E2E builds only — `dev_env`; a shipped build
/// must not let the environment choose where its reports go), then the compile-time embedded value
/// (official builds). Cached. `None`/blank ⇒ telemetry disabled everywhere.
fn raw_connection() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            crate::dev_env::var(ENV_VAR)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| option_env!("TEXTREE_TELEMETRY_CONNECTION").map(str::to_string))
                .filter(|s| !s.trim().is_empty())
        })
        .clone()
}

/// The variable the AI helper is told its reports folder in — this process's own `reports`
/// folder, so the helper's failures are one more segment of the same stream, verified against the
/// same registry and sent from here. The helper never picks a folder, and never sends.
pub const HOST_REPORTS_VAR: &str = "TEXTREE_REPORTS_DIR";

/// Where this process keeps its reports, under the app's log folder.
pub fn reports_dir(log_dir: &Path) -> std::path::PathBuf {
    log_dir.join("reports")
}

/// Starts recording failures into `log_dir`, and — when telemetry is on — sends in the background
/// what earlier launches recorded and could not send, and this launch's `app.launch`. Never blocks
/// and never fails: telemetry that cannot work stays quiet.
pub fn start(log_dir: &Path) {
    let dir = reports_dir(log_dir);
    // An earlier release kept one `reports.jsonl`; what it had not sent still goes, once.
    let _ = adopt(
        &dir,
        &log_dir.join("reports.jsonl"),
        &log_dir.join("reports.sent"),
    );
    // Before this launch writes or sends anything: the folder is evidence, not an archive.
    let _ = tidy(&dir, MAX_STREAM_BYTES);
    if let Ok(segment) = Segment::open(&dir) {
        let _ = REPORTER.set(Reporter::new(segment.file()));
        let _ = SEGMENT.set(segment);
    }

    let Some(sink) = raw_connection().as_deref().and_then(Sink::parse) else {
        return;
    };
    // Transparency: what is sent is named in the app's own log, rather than leaving unannounced.
    log::info!(
        "[telemetry] sending app.launch and the failures recorded in {}",
        dir.display()
    );
    let _ = std::thread::Builder::new().spawn(move || {
        let agent = Sink::agent();
        let body = envelope::launch_envelope(
            &LaunchEvent,
            &sink.instrumentation_key,
            &EnvFacts::current(),
            &envelope::format_rfc3339_utc(now_secs()),
        );
        let _ = agent
            .post(&sink.track_url)
            .send_json(serde_json::json!([body]));
        let _ = sink.send_pending(&agent, &REGISTRY, &dir);
    });
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A panic, by where it happened in our own source — never its payload. `location` is the panic's
/// `file:line:column`; a location outside our source (a dependency's, which is an absolute path
/// into the build machine's registry) is left out of the report.
pub fn panic_report(location: &str) -> Report {
    Report::new(&SHELL, "Panic", location, VERSION)
}

/// The default notes folder could not be made: the OS error number only, never the message.
pub fn vault_open_failed_report(os_error_code: Option<i32>) -> Report {
    match os_error_code {
        Some(code) => SHELL.report(
            "VaultOpenFailed",
            "",
            VERSION,
            &[("os_error_code", &code.to_string())],
        ),
        None => SHELL.report("VaultOpenFailed", "", VERSION, &[]),
    }
}

/// Records a failure, once per kind and place per launch. Never blocks on the network and never
/// fails: it only appends a line to a file, and a failure to do even that is dropped.
pub fn record(report: Report) {
    if let Some(reporter) = REPORTER.get() {
        let _ = reporter.record(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_in_our_source_keeps_its_place_and_nothing_else() {
        let r = panic_report("src\\commands.rs:42:9");
        assert_eq!(r.kind, "Panic");
        assert_eq!(r.layer, "textree-desktop");
        assert_eq!(r.frames, vec!["commands.rs:42:9".to_string()]);
    }

    #[test]
    fn a_panic_outside_our_source_carries_no_path() {
        let r = panic_report(
            // forbidden-tokens: allow absolute-path — the test is that a registry path never leaves the machine
            "C:\\Users\\someone\\.cargo\\registry\\src\\index.crates.io-0\\dep-1.0.0\\src\\lib.rs:7:1",
        );
        assert!(r.frames.is_empty(), "{:?}", r.frames);
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("someone") && !json.contains("registry"),
            "{json}"
        );
    }

    #[test]
    fn a_folder_that_cannot_be_made_carries_only_its_error_number() {
        let r = vault_open_failed_report(Some(3));
        assert_eq!(r.kind, "VaultOpenFailed");
        assert!(r.frames.is_empty());
        assert_eq!(
            r.details.get("os_error_code").map(String::as_str),
            Some("3")
        );
        assert!(vault_open_failed_report(None).details.is_empty());
    }

    /// Once started, a failure is a line in the file the person can read; the same failure twice in
    /// one launch is one line. (The only test that calls `start`: the reporter is set once per
    /// process.)
    #[test]
    fn after_start_a_failure_is_one_line_in_the_reports_file() {
        let dir = tempfile::tempdir().unwrap();
        start(dir.path());
        record(vault_open_failed_report(Some(5)));
        record(vault_open_failed_report(Some(5)));
        let segment = std::fs::read_dir(dir.path().join("reports"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .expect("this launch's segment");
        let lines = std::fs::read_to_string(segment).unwrap();
        assert_eq!(lines.lines().count(), 1, "{lines}");
        assert!(
            lines.contains("\"VaultOpenFailed\"") && lines.contains("\"os_error_code\":\"5\""),
            "{lines}"
        );
    }

    /// A report the AI helper writes (its own test writes the same line — `FailureReportsTests`)
    /// verifies against this registry, so it is sent with the shell's own.
    #[test]
    fn a_report_of_the_ai_helper_verifies_against_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("host.jsonl");
        std::fs::copy("src/telemetry/host-report.golden.jsonl", &file).unwrap();
        let pending =
            tauri_kit_diagnostics::pending_reports(&REGISTRY, &file, &dir.path().join("host.sent"))
                .unwrap();
        let reports: Vec<_> = pending.into_iter().flat_map(|p| p.items).collect();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].layer, "textree-host");
        assert_eq!(
            reports[0].details.get("event").map(String::as_str),
            Some("model.download.failed")
        );
    }

    /// Sends this launch's event and a recorded failure to a real Application Insights resource:
    /// `TEXTREE_TELEMETRY_CONNECTION=… cargo test --lib reaches_application_insights -- --ignored`.
    #[test]
    #[ignore = "needs an Application Insights resource"]
    fn reaches_application_insights() {
        let sink = Sink::parse(&std::env::var(ENV_VAR).unwrap()).expect("a connection string");
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("reports");
        let segment = Segment::open(&reports).unwrap();
        Reporter::new(segment.file())
            .record(panic_report("src/lib.rs:1:1"))
            .unwrap();
        let agent = Sink::agent();
        let body = envelope::launch_envelope(
            &LaunchEvent,
            &sink.instrumentation_key,
            &EnvFacts::current(),
            &envelope::format_rfc3339_utc(now_secs()),
        );
        let answer = agent
            .post(&sink.track_url)
            .send_json(serde_json::json!([body]))
            .unwrap();
        assert_eq!(answer.status(), 200);
        assert_eq!(sink.send_pending(&agent, &REGISTRY, &reports).unwrap(), 1);
    }
}

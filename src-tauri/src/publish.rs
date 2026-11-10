//! In-app publishing: spawn the canopy renderer (a separate executable) to turn the open vault
//! into a static site. Read-only over the source (publish is one-directional):
//! canopy only *reads* the vault; the source `.md` is never mutated. User-triggered only — there is
//! no autonomous publish (Filer boundary).
//!
//! Layering: this module owns the validation + spawn orchestration; `commands::publish_site` is the
//! thin IPC wrapper that resolves how to invoke canopy and forwards here.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
use tauri_kit_sidecar::Ran;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishOptions {
    /// Overrides the site title (defaults to the vault folder name inside canopy).
    pub site_title: Option<String>,
    /// The host's design-token CSS **content** (not a path). Written to a temp file and passed to
    /// canopy via `--stylesheet` (linked after canopy's own, so its values win) so the published
    /// site matches the app. None = canopy built-in tokens.
    pub tokens_css: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub page_count: usize,
    pub out_dir: String,
}

/// Overall deadline for a canopy render. Bounds a hung renderer so publish can never wedge forever.
/// A normal vault renders in seconds; 120s is a generous ceiling (mirrors the ask chat timeout).
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

/// How to invoke canopy: a program plus any fixed leading args. In production
/// (`canopy_from_resource_dir`) the program is the bundled pinned `node` runtime and `prefix_args`
/// holds canopy's CLI script path; in dev/E2E (`TEXTREE_CANOPY_CLI`) it is `node` with the CLI script
/// path, or a standalone exe.
pub struct CanopyInvocation {
    pub program: OsString,
    pub prefix_args: Vec<OsString>,
}

/// Where canopy's command-line entry sits in the bundled payload: the pinned npm release installed
/// as it is published (its `bin`). `scripts/canopy-stage.mjs` names the same path.
const CANOPY_CLI_IN_PAYLOAD: [&str; 5] = ["node_modules", "@iyulab", "canopy", "dist", "cli.js"];

/// Resolves the bundled canopy sidecar (mechanism B) from a Tauri resource directory. The payload
/// lives under `<resource>/canopy/`: a pinned `node` runtime plus the installed canopy release.
/// Returns the invocation `node <cli>`, or `None` if either piece is missing (e.g. an unbundled dev
/// build).
pub fn canopy_from_resource_dir(resource: &Path) -> Option<CanopyInvocation> {
    let dir = resource.join("canopy");
    let node = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
    let cli = CANOPY_CLI_IN_PAYLOAD
        .iter()
        .fold(dir.clone(), |p, part| p.join(part));
    if node.exists() && cli.exists() {
        Some(CanopyInvocation {
            program: node.into_os_string(),
            prefix_args: vec![cli.into_os_string()],
        })
    } else {
        None
    }
}

/// Canonicalizes the longest existing ancestor of `p` and re-appends the non-existing tail, so a
/// not-yet-created output directory can still be compared against the vault for containment.
fn resolve_existing_prefix(p: &Path) -> Result<PathBuf, String> {
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = p.to_path_buf();
    loop {
        if current.exists() {
            let mut base = current.canonicalize().map_err(|e| e.to_string())?;
            for part in tail.iter().rev() {
                base.push(part);
            }
            return Ok(base);
        }
        let name = current.file_name().map(|s| s.to_os_string());
        let parent = current.parent().map(|p| p.to_path_buf());
        match (parent, name) {
            (Some(parent), Some(n)) if parent != current => {
                tail.push(n);
                current = parent;
            }
            _ => return Err("the output path has no existing parent directory".into()),
        }
    }
}

/// Validates the publish source/destination. The vault must be a real directory, and the output
/// must live entirely **outside** the vault — otherwise canopy would re-ingest its own output and
/// the published site could clobber (or be polluted by) the source of truth.
pub fn validate_publish_paths(vault: &Path, out: &Path) -> Result<(), String> {
    if !vault.is_dir() {
        return Err("the vault path is not a directory".into());
    }
    let vault_c = vault.canonicalize().map_err(|e| e.to_string())?;
    let out_c = resolve_existing_prefix(out)?;
    if out_c == vault_c || out_c.starts_with(&vault_c) {
        return Err("the output directory must be outside the vault".into());
    }
    if vault_c.starts_with(&out_c) {
        return Err("the output directory must not contain the vault".into());
    }
    Ok(())
}

/// The notes (`.md`, any case) in the folder, relative and `/`-separated, sorted. Folders whose
/// name starts with `.` and `node_modules` are not entered; files whose name starts with `.` and
/// anything that is not a regular file are passed over.
pub fn outgoing_notes(vault: &Path) -> io::Result<Vec<String>> {
    fn walk(dir: &Path, rel: &str, found: &mut Vec<String>) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !name.starts_with('.') && name != "node_modules" {
                    // One unreadable subfolder must not hide the notes in the others.
                    if let Err(e) = walk(&entry.path(), &child, found) {
                        log::warn!("publish: skipping unreadable folder {child}: {e}");
                    }
                }
            } else if kind.is_file()
                && !name.starts_with('.')
                && crate::note_refs::is_note_path(&name)
            {
                found.push(child);
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(vault, "", &mut found)?;
    found.sort();
    Ok(found)
}

/// The arguments after the renderer's own prefix: build `vault` into `out`.
fn canopy_args(
    vault: &Path,
    out: &Path,
    options: &PublishOptions,
    tokens_css: Option<&Path>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["build".into(), vault.into(), out.into()];
    if let Some(title) = &options.site_title {
        args.push("--site-title".into());
        args.push(title.into());
    }
    if let Some(path) = tokens_css {
        args.push("--stylesheet".into());
        args.push(path.into());
    }
    args
}

/// What the renderer prints for a published file whose path it writes itself (a page's `.html`,
/// its stylesheets): `the vault publishes "<file>", where canopy writes …`.
const COLLISION_MARK: &str = "the vault publishes \"";

/// The files the build refused because the renderer writes to their paths itself, named in plain
/// words — or None when the build failed for another reason.
fn collision_message(stderr: &str) -> Option<String> {
    let files: Vec<&str> = stderr
        .lines()
        .filter_map(|line| {
            let rest = &line[line.find(COLLISION_MARK)? + COLLISION_MARK.len()..];
            Some(&rest[..rest.find('"')?])
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    let named = files
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "The published site needs these names for its own files: {named}. Rename or move {}, then publish again.",
        if files.len() == 1 { "it" } else { "them" }
    ))
}

/// Counts `.html` files in the output tree — the number of published pages reported back.
pub fn count_html_pages(out: &Path) -> usize {
    fn walk(dir: &Path, acc: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, acc);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("html"))
            {
                *acc += 1;
            }
        }
    }
    let mut count = 0;
    walk(out, &mut count);
    count
}

/// Runs `cmd` and waits up to `timeout` for it to exit, through the kit's `run`: no console window,
/// stdin closed, both streams read while it is waited for (a chatty child cannot fill a pipe and
/// stall), and on timeout the whole process tree is stopped — not only the child, whose own children
/// would otherwise live on holding the pipes — and the call returns at once.
fn spawn_bounded(cmd: Command, timeout: Duration) -> Result<Output, String> {
    match tauri_kit_sidecar::run(cmd, timeout)
        .map_err(|e| format!("failed to start canopy: {e}"))?
    {
        Ran::Exited(output) => Ok(output),
        Ran::TimedOut => Err(format!("rendering timed out after {}s", timeout.as_secs())),
    }
}

/// Validates, then spawns canopy to build the site. Returns the page count and output directory.
/// The source vault is read-only throughout (canopy never writes into it).
pub fn run_publish(
    vault: &Path,
    source: &Path,
    out: &Path,
    options: &PublishOptions,
    canopy: &CanopyInvocation,
    timeout: Duration,
) -> Result<PublishResult, String> {
    validate_publish_paths(vault, out)?;

    // Materialize the injected tokens CSS to a temp file (canopy reads it via --stylesheet). Held in
    // scope until canopy finishes so the path stays valid; auto-removed on drop.
    let tokens_file = match &options.tokens_css {
        Some(css) => {
            let mut f = tempfile::Builder::new()
                .suffix(".css")
                .tempfile()
                .map_err(|e| e.to_string())?;
            f.write_all(css.as_bytes()).map_err(|e| e.to_string())?;
            Some(f)
        }
        None => None,
    };

    let mut cmd = Command::new(&canopy.program);
    cmd.args(&canopy.prefix_args);
    cmd.args(canopy_args(
        source,
        out,
        options,
        tokens_file.as_ref().map(|f| f.path()),
    ));
    let output = spawn_bounded(cmd, timeout)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(message) = collision_message(&stderr) {
            return Err(message);
        }
        return Err(format!("canopy failed: {}", stderr.trim()));
    }

    Ok(PublishResult {
        page_count: count_html_pages(out),
        out_dir: out.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_file_on_a_path_the_renderer_writes_is_named_in_plain_words() {
        let stderr = "canopy: the vault publishes \"tokens.css\", where canopy writes its design tokens — rename or move it\n\
                      canopy: the vault publishes \"guide.html\", where canopy writes the page rendered from guide.md — rename or move it\n";
        assert_eq!(
            collision_message(stderr).as_deref(),
            Some("The published site needs these names for its own files: \"tokens.css\", \"guide.html\". Rename or move them, then publish again.")
        );
        let one = "canopy: the vault publishes \"styles.css\", where canopy writes its stylesheet — rename or move it";
        assert!(collision_message(one)
            .unwrap()
            .ends_with("Rename or move it, then publish again."));
        assert_eq!(collision_message("canopy: something else went wrong"), None);
    }

    /// The render wait must be bounded: a child that outlives the deadline is killed and the call
    /// returns promptly with a timeout error — it must NOT wait for the child to finish on its own.
    #[test]
    #[cfg(windows)]
    fn spawn_bounded_kills_a_hung_child() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        // `ping -n 31 127.0.0.1` runs for ~30 seconds. Our deadline is 200ms. The gap is what the
        // test measures — returning long before the child would have ended on its own — so it has
        // to be wide enough that a loaded machine starting the child slowly cannot close it.
        // Started directly rather than through `cmd /C`, so killing it ends it: a grandchild
        // would outlive the kill and run its thirty seconds in the background.
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "31", "127.0.0.1"]);

        let start = Instant::now();
        let result = spawn_bounded(cmd, Duration::from_millis(200));

        assert!(
            result.is_err(),
            "a child outliving the deadline must return an error"
        );
        assert!(
            result.unwrap_err().to_lowercase().contains("timed out"),
            "the error should say it timed out"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "must kill and return near the 200ms deadline, not wait ~30s for ping: took {:?}",
            start.elapsed()
        );
    }

    /// A render that starts helpers of its own is bounded too: the whole tree is stopped, so the
    /// call returns near the deadline instead of waiting on a grandchild that holds the pipes open.
    #[test]
    #[cfg(windows)]
    fn spawn_bounded_stops_what_a_hung_child_started() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        // `cmd /C` starts `ping` as a grandchild, which runs ~30 seconds.
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 31 127.0.0.1"]);

        let start = Instant::now();
        let result = spawn_bounded(cmd, Duration::from_millis(300));

        assert!(result.unwrap_err().to_lowercase().contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the grandchild must not hold the call for its ~30s: took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn validate_rejects_non_directory_vault() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("not-a-dir.md");
        std::fs::write(&file, "x").unwrap();
        assert!(validate_publish_paths(&file, &tmp.path().join("site")).is_err());
    }

    #[test]
    fn validate_rejects_output_inside_vault() {
        let tmp = TempDir::new().unwrap();
        // out = <vault>/site — canopy would re-read its own output / write into the source.
        assert!(validate_publish_paths(tmp.path(), &tmp.path().join("site")).is_err());
    }

    #[test]
    fn validate_rejects_output_equal_to_vault() {
        let tmp = TempDir::new().unwrap();
        assert!(validate_publish_paths(tmp.path(), tmp.path()).is_err());
    }

    #[test]
    fn validate_rejects_output_containing_vault() {
        let parent = TempDir::new().unwrap();
        let vault = parent.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        // out = the parent that contains the vault → publishing there could clobber the vault.
        assert!(validate_publish_paths(&vault, parent.path()).is_err());
    }

    #[test]
    fn validate_accepts_sibling_output_dir() {
        let parent = TempDir::new().unwrap();
        let vault = parent.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        // out = a sibling that does not exist yet (parent exists) → allowed.
        assert!(validate_publish_paths(&vault, &parent.path().join("site")).is_ok());
    }

    #[test]
    fn count_html_pages_counts_recursively_and_ignores_others() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("index.html"), "").unwrap();
        std::fs::write(tmp.path().join("tokens.css"), "").unwrap();
        let sub = tmp.path().join("notes");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("idea.html"), "").unwrap();
        std::fs::write(sub.join("image.png"), "").unwrap();
        assert_eq!(count_html_pages(tmp.path()), 2);
    }

    fn put(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn outgoing_notes_walks_the_vault_as_the_renderer_does() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        for rel in [
            "a.md",
            "Upper.MD",
            "sub/deep/b.md",
            "sub/image.png",
            ".env",
            "sub/.DS_Store",
            ".hidden-note.md",
            ".git/config",
            ".obsidian/x.md",
            "node_modules/pkg/readme.md",
            "sub/node_modules/y.md",
        ] {
            put(root, rel);
        }

        assert_eq!(
            outgoing_notes(root).unwrap(),
            vec!["Upper.MD", "a.md", "sub/deep/b.md"]
        );
    }

    #[test]
    #[cfg(windows)]
    fn outgoing_notes_passes_over_a_symbolic_link() {
        let tmp = TempDir::new().unwrap();
        put(tmp.path(), "a.md");
        let target = TempDir::new().unwrap();
        put(target.path(), "outside.md");
        let file_link = std::os::windows::fs::symlink_file(
            target.path().join("outside.md"),
            tmp.path().join("linked.md"),
        );
        let dir_link =
            std::os::windows::fs::symlink_dir(target.path(), tmp.path().join("linked-dir"));
        if file_link.is_err() || dir_link.is_err() {
            eprintln!("skipped: this account may not create symbolic links");
            return;
        }
        assert_eq!(outgoing_notes(tmp.path()).unwrap(), vec!["a.md"]);
    }

    #[test]
    fn canopy_args_carry_the_title_and_tokens_and_nothing_else() {
        let options = PublishOptions {
            site_title: Some("Site".into()),
            tokens_css: None,
        };
        let args = canopy_args(
            Path::new("v"),
            Path::new("o"),
            &options,
            Some(Path::new("t.css")),
        );
        let expected: Vec<OsString> = [
            "build",
            "v",
            "o",
            "--site-title",
            "Site",
            "--stylesheet",
            "t.css",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn canopy_from_resource_dir_finds_bundled_node_and_cli() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("canopy");
        std::fs::create_dir(&dir).unwrap();
        let node_name = if cfg!(windows) { "node.exe" } else { "node" };
        std::fs::write(dir.join(node_name), "").unwrap();
        let cli = dir
            .join("node_modules")
            .join("@iyulab")
            .join("canopy")
            .join("dist")
            .join("cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "").unwrap();

        let inv = canopy_from_resource_dir(tmp.path()).expect("should resolve");
        assert_eq!(inv.program, dir.join(node_name).into_os_string());
        assert_eq!(inv.prefix_args, vec![cli.into_os_string()]);
    }

    #[test]
    fn canopy_from_resource_dir_none_when_incomplete() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("canopy");
        std::fs::create_dir(&dir).unwrap();
        // The CLI present but node missing -> not resolvable.
        let cli = dir
            .join("node_modules")
            .join("@iyulab")
            .join("canopy")
            .join("dist")
            .join("cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "").unwrap();
        assert!(canopy_from_resource_dir(tmp.path()).is_none());
    }

    /// Production-path guard: resolve canopy from the *assembled* sidecar payload (node + the
    /// installed canopy release under `src-tauri/resources/canopy/`) and actually publish a vault through it,
    /// proving the bundled payload renders AND leaves the source `.md` byte-unchanged. Ignored
    /// by default because it requires the payload — run `scripts/assemble-canopy-sidecar.ps1` first,
    /// then `cargo test -- --ignored run_publish_via_assembled_sidecar`. CI does both (release.yml).
    #[test]
    #[ignore = "requires assembled canopy sidecar payload (run scripts/assemble-canopy-sidecar.ps1)"]
    fn run_publish_via_assembled_sidecar() {
        // resource dir = src-tauri/resources (the helper appends `canopy/`).
        let resource = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
        let canopy = canopy_from_resource_dir(&resource)
            .expect("assembled payload missing — run scripts/assemble-canopy-sidecar.ps1");

        let tmp = TempDir::new().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        let note = vault.join("hello.md");
        let source = "# Hello\n\nworld\n";
        std::fs::write(&note, source).unwrap();
        std::fs::write(vault.join(".env"), "SECRET=1\n").unwrap();
        let out = tmp.path().join("site");

        let result = run_publish(
            &vault,
            &vault,
            &out,
            &PublishOptions {
                site_title: None,
                tokens_css: None,
            },
            &canopy,
            RENDER_TIMEOUT,
        )
        .expect("publish should succeed via the assembled sidecar");

        assert!(
            result.page_count >= 1,
            "expected at least one published page"
        );
        assert!(
            out.join("hello.html").exists(),
            "expected hello.html in the output"
        );
        assert!(
            !out.join(".env").exists(),
            "a hidden file must not reach the site"
        );
        // The source vault note is untouched.
        assert_eq!(std::fs::read_to_string(&note).unwrap(), source);
    }

    /// Runtime integration: actually spawn canopy (via node) and prove the Rust wiring end-to-end —
    /// the arg vector, the temp-file -> subprocess handoff (a Windows file-sharing risk), exit
    /// status, AND that the source vault is byte-unchanged and read-only. Ignored by default
    /// because it needs node + a built canopy; run with `cargo test -- --ignored`. The permanent
    /// guard is the C44 E2E; this is the empirical de-risk before building UI on top.
    #[test]
    #[ignore = "requires node + a built canopy CLI"]
    fn run_publish_spawns_canopy_and_keeps_source_unchanged() {
        // Note: do NOT canonicalize — on Windows that yields a `\\?\` verbatim path that node's
        // module resolver chokes on. Production never canonicalizes the paths it hands canopy.
        let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../canopy/dist/cli.js");
        assert!(
            cli.exists(),
            "build canopy first (npm run build in canopy/)"
        );

        let parent = TempDir::new().unwrap();
        let vault = parent.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        let note = vault.join("note.md");
        let source = "# Hello\r\n\r\nworld\r\n"; // CRLF too: the source must come back byte-identical
        std::fs::write(&note, source).unwrap();
        let out = parent.path().join("site");

        let options = PublishOptions {
            site_title: Some("Spawn Test".into()),
            tokens_css: Some(":root{--probe:INJECTED}\n".into()),
        };
        let canopy = CanopyInvocation {
            program: "node".into(),
            prefix_args: vec![cli.into_os_string()],
        };

        let result = run_publish(&vault, &vault, &out, &options, &canopy, RENDER_TIMEOUT)
            .expect("publish should succeed");

        assert!(result.page_count > 0, "at least one page emitted");
        assert!(out.join("note.html").is_file(), "the note rendered to html");
        let tokens = std::fs::read_to_string(out.join("tokens.css")).unwrap();
        assert!(
            tokens.contains("INJECTED"),
            "injected tokens reached the site"
        );
        // The source `.md` is read-only — its bytes (CRLF included) are untouched by publish.
        assert_eq!(std::fs::read_to_string(&note).unwrap(), source);
    }
}

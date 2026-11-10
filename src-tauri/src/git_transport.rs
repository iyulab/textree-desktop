//! Git's smart HTTP protocol, carried by the app's own HTTP client.
//!
//! libgit2 is built without its `https` and `ssh` transports (see `tests/dependency_policy.rs`),
//! so a remote needs a transport from somewhere. This module registers one: libgit2's smart
//! protocol layer still does the negotiation, pkt-lines and packs; what lives here is only the
//! channel underneath it — one HTTP request per service, and the credentials to send with it.
//!
//! Credentials never come from the user's git configuration or credential helpers. A caller
//! states them for the duration of one operation with [`with_credentials`], which keeps what the
//! app sends to a remote entirely in the app's hands.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::sync::{Once, OnceLock};
use std::time::Duration;

use base64::Engine;
use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::{Error, Remote};

/// How long connecting, or waiting for the server to start answering, may take. A pack can take
/// much longer than this to arrive, so the body itself is not bounded by it.
const WAIT: Duration = Duration::from_secs(30);

/// How long the server may send nothing at all, at any point of an exchange, before it is taken
/// for gone. A pack may take as long as it takes while it keeps arriving; a connection that has
/// died without closing (a computer gone to sleep, a dropped route) would otherwise be waited on
/// for good, and the folder would stop being backed up until the app restarts.
#[cfg(not(test))]
const IDLE: Duration = Duration::from_secs(60);
#[cfg(test)]
const IDLE: Duration = Duration::from_secs(5);

/// The HTTP client every exchange goes through: ureq's own connectors, with each wait for the
/// server cut off at [`IDLE`].
fn agent() -> &'static ureq::Agent {
    use ureq::unversioned::resolver::DefaultResolver;
    use ureq::unversioned::transport::{Connector, DefaultConnector};
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::with_parts(
            ureq::Agent::config_builder().build(),
            DefaultConnector::new().chain(Idle),
            DefaultResolver::default(),
        )
    })
}

/// Wraps the connection ureq makes so that no single wait on it outlasts [`IDLE`]. ureq bounds a
/// whole phase (connecting, the response head, the body); this bounds the silence within one, so
/// a body that keeps arriving is never cut, and one that stops is.
///
/// TODO(upstream: ureq has no limit on the silence within a phase — drop this wrapper, and the
/// `unversioned` transport API it needs, when it grows one)
#[derive(Debug)]
struct Idle;

impl ureq::unversioned::transport::Connector<Box<dyn ureq::unversioned::transport::Transport>>
    for Idle
{
    type Out = IdleTransport;

    fn connect(
        &self,
        _details: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<Box<dyn ureq::unversioned::transport::Transport>>,
    ) -> Result<Option<IdleTransport>, ureq::Error> {
        Ok(chained.map(IdleTransport))
    }
}

#[derive(Debug)]
struct IdleTransport(Box<dyn ureq::unversioned::transport::Transport>);

/// The phase's own deadline, or [`IDLE`] from now when that comes first.
fn within_idle(
    timeout: ureq::unversioned::transport::NextTimeout,
) -> ureq::unversioned::transport::NextTimeout {
    let idle = ureq::unversioned::transport::time::Duration::from(IDLE);
    if timeout.after > idle {
        ureq::unversioned::transport::NextTimeout {
            after: idle,
            reason: timeout.reason,
        }
    } else {
        timeout
    }
}

impl ureq::unversioned::transport::Transport for IdleTransport {
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.0.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<(), ureq::Error> {
        self.0.transmit_output(amount, within_idle(timeout))
    }

    fn await_input(
        &mut self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<bool, ureq::Error> {
        self.0.await_input(within_idle(timeout))
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }

    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

/// The body of an answer, with a failure partway through said the way every other failure to reach
/// the remote is.
struct Answer(Box<dyn Read + Send>);

impl Read for Answer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .read(buf)
            .map_err(|e| io::Error::new(e.kind(), format!("could not reach the remote: {e}")))
    }
}

/// What to send a remote to prove who is asking.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub secret: String,
}

thread_local! {
    // libgit2 calls back into the transport on the thread that started the operation, so the
    // credentials an operation was given are exactly the ones in effect on that thread.
    static CURRENT: RefCell<Option<Credentials>> = const { RefCell::new(None) };
}

/// Runs `op` with `credentials` sent on every request it makes to a remote.
pub fn with_credentials<T>(credentials: Option<Credentials>, op: impl FnOnce() -> T) -> T {
    struct Restore(Option<Credentials>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|c| c.replace(credentials));
    let _restore = Restore(previous);
    op()
}

/// Makes `http://` and `https://` remotes reachable. Idempotent; call before any remote operation.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // A scheme, not a prefix: libgit2 appends "://" itself, so "http" does not claim https.
        for scheme in ["https", "http"] {
            // SAFETY: registration is serialised by `Once`, and no transport is being created
            // while it runs — nothing reaches a remote before `install` has returned.
            let registered = unsafe { git2::transport::register(scheme, factory) };
            if let Err(e) = registered {
                log::error!("git transport for {scheme} could not be registered: {e}");
            }
        }
    });
}

fn factory(remote: &Remote<'_>) -> Result<Transport, Error> {
    Transport::smart(remote, true, HttpSubtransport)
}

/// Plain HTTP carries the credentials in the clear, so it is only allowed to this machine.
pub fn check_url(url: &str) -> Result<(), Error> {
    if url.starts_with("https://") {
        return Ok(());
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(Error::from_str("only http and https remotes are supported"));
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        Ok(())
    } else {
        Err(Error::from_str(
            "this remote uses http, which would send your credentials unencrypted — use https",
        ))
    }
}

struct HttpSubtransport;

impl SmartSubtransport for HttpSubtransport {
    fn action(
        &self,
        url: &str,
        action: Service,
    ) -> Result<Box<dyn SmartSubtransportStream>, Error> {
        check_url(url)?;
        let base = url.trim_end_matches('/');
        let (service, request) = match action {
            Service::UploadPackLs => ("git-upload-pack", None),
            Service::ReceivePackLs => ("git-receive-pack", None),
            Service::UploadPack => ("git-upload-pack", Some(Vec::new())),
            Service::ReceivePack => ("git-receive-pack", Some(Vec::new())),
        };
        Ok(Box::new(HttpStream {
            base: base.to_string(),
            service,
            request,
            credentials: CURRENT.with(|c| c.borrow().clone()),
            response: None,
        }))
    }

    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// One exchange with the remote. An advertisement (`request` is `None`) is a GET; a service call
/// collects what libgit2 writes and sends it as one POST when libgit2 first asks for the answer.
struct HttpStream {
    base: String,
    service: &'static str,
    request: Option<Vec<u8>>,
    credentials: Option<Credentials>,
    response: Option<Box<dyn Read + Send>>,
}

impl HttpStream {
    fn send(&mut self) -> io::Result<Box<dyn Read + Send>> {
        let service = self.service;
        let authorization = self.credentials.as_ref().map(|c| {
            let pair = format!("{}:{}", c.username, c.secret);
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            )
        });
        let request = self.request.take();
        let advertisement = request.is_none();
        let result = match request {
            None => {
                let url = format!("{}/info/refs?service={service}", self.base);
                // No `Git-Protocol` header: a server that honours it answers with a leading
                // "version 1" line, which libgit2's parser does not accept.
                let mut req = agent().get(&url);
                if let Some(auth) = &authorization {
                    req = req.header("Authorization", auth);
                }
                req.config()
                    .http_status_as_error(false)
                    .timeout_connect(Some(WAIT))
                    .timeout_recv_response(Some(WAIT))
                    .build()
                    .call()
            }
            Some(body) => {
                let url = format!("{}/{service}", self.base);
                let mut req = agent()
                    .post(&url)
                    .header("Content-Type", format!("application/x-{service}-request"))
                    .header("Accept", format!("application/x-{service}-result"));
                if let Some(auth) = &authorization {
                    req = req.header("Authorization", auth);
                }
                req.config()
                    .http_status_as_error(false)
                    .timeout_connect(Some(WAIT))
                    .timeout_recv_response(Some(WAIT))
                    .build()
                    .send(&body[..])
            }
        };
        let response =
            result.map_err(|e| io::Error::other(format!("could not reach the remote: {e}")))?;
        let status = response.status().as_u16();
        match status {
            200 => {}
            401 | 403 => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the remote refused these credentials",
                ))
            }
            404 => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no repository at this address",
                ))
            }
            _ => {
                return Err(io::Error::other(format!(
                    "the remote answered with status {status}"
                )))
            }
        }
        // A server that answers without the smart content type is speaking the old "dumb"
        // protocol (or is not a git server at all); libgit2 cannot use what it would send.
        let expected = format!(
            "application/x-{service}-{}",
            if advertisement {
                "advertisement"
            } else {
                "result"
            }
        );
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.starts_with(&expected) {
            return Err(io::Error::other(
                "this address does not serve a git repository over smart HTTP",
            ));
        }
        Ok(Box::new(Answer(Box::new(
            response.into_body().into_reader(),
        ))))
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.response.is_none() {
            let reader = self.send()?;
            self.response = Some(reader);
        }
        self.response
            .as_mut()
            .expect("response set above")
            .read(buf)
    }
}

impl Write for HttpStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match (&mut self.request, &self.response) {
            (Some(body), None) => {
                body.extend_from_slice(data);
                Ok(data.len())
            }
            _ => Err(io::Error::other(
                "nothing more can be sent on this exchange",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A smart HTTP server over `git http-backend`, for tests anywhere in the crate that need a real
/// remote reached through this transport.
#[cfg(test)]
pub(crate) mod test_server {
    use base64::Engine;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    pub const USER: &str = "someone";
    pub const SECRET: &str = "right-token";

    /// A smart HTTP server for the bare repositories under `root`: `git http-backend` behind a
    /// minimal HTTP/1.1 front that insists on the credentials above. `None` when git is absent.
    pub fn serve(root: PathBuf) -> Option<String> {
        let probe = Command::new("git").arg("--version").output().ok()?;
        if !probe.status.success() {
            return None;
        }
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let root = root.clone();
                std::thread::spawn(move || {
                    let _ = answer(stream, &root);
                });
            }
        });
        Some(format!("http://127.0.0.1:{port}/repo.git"))
    }

    fn answer(stream: TcpStream, root: &Path) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let mut length = 0usize;
        let mut content_type = String::new();
        let mut authorization = String::new();
        let mut protocol = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((k, v)) = header.split_once(':') {
                match k.to_ascii_lowercase().as_str() {
                    "content-length" => length = v.trim().parse().unwrap_or(0),
                    "content-type" => content_type = v.trim().to_string(),
                    "authorization" => authorization = v.trim().to_string(),
                    "git-protocol" => protocol = v.trim().to_string(),
                    _ => {}
                }
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let mut out = stream;

        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{SECRET}"))
        );
        if authorization != expected {
            out.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
            return Ok(());
        }

        let mut child = Command::new("git")
            .arg("http-backend")
            .env("GIT_PROJECT_ROOT", root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("REQUEST_METHOD", &method)
            .env("PATH_INFO", path)
            .env("QUERY_STRING", query)
            .env("CONTENT_TYPE", &content_type)
            .env("CONTENT_LENGTH", length.to_string())
            .env("REMOTE_USER", USER)
            // As a hosted server does: a client that asks for a protocol version gets it.
            .env("HTTP_GIT_PROTOCOL", &protocol)
            .env("REMOTE_ADDR", "127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        child.stdin.take().expect("piped").write_all(&body)?;
        let raw = child.wait_with_output()?.stdout;
        let (end, gap) = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)))
            .unwrap_or((raw.len(), 0));
        let head = String::from_utf8_lossy(&raw[..end]).to_string();
        let content = &raw[(end + gap).min(raw.len())..];
        let mut status = "200 OK".to_string();
        let mut headers = String::new();
        for h in head.lines() {
            match h.split_once(':') {
                Some((k, v)) if k.eq_ignore_ascii_case("status") => status = v.trim().to_string(),
                Some((k, v)) => headers.push_str(&format!("{k}: {}\r\n", v.trim())),
                None => {}
            }
        }
        let start = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            content.len()
        );
        out.write_all(start.as_bytes())?;
        out.write_all(content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{serve, SECRET, USER};
    use super::*;
    use std::path::Path;
    const NOTES: &str = "refs/textree/notes:refs/textree/notes";

    fn right() -> Option<Credentials> {
        Some(Credentials {
            username: USER.into(),
            secret: SECRET.into(),
        })
    }

    /// A repository with one note recorded on the app's own reference.
    fn recorded(dir: &Path, text: &[u8]) -> (git2::Repository, git2::Oid) {
        let repo = git2::Repository::init(dir).unwrap();
        let id = {
            let blob = repo.blob(text).unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder.insert("manual.md", blob, 0o100644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = git2::Signature::now("a", "a@example.com").unwrap();
            repo.commit(
                Some(crate::git_engine::NOTES_REF),
                &sig,
                &sig,
                "first",
                &tree,
                &[],
            )
            .unwrap()
        };
        (repo, id)
    }

    /// A server that reads one request and answers with `head`, then sends `body` a byte at a time
    /// with `gap` between bytes — or, with no gap, sends nothing more and keeps the connection open.
    fn answering(head: &'static [u8], body: &'static [u8], gap: Option<Duration>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let mut seen: Vec<u8> = Vec::new();
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => seen.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = s.write_all(head);
                    match gap {
                        Some(gap) => {
                            for byte in body {
                                std::thread::sleep(gap);
                                if s.write_all(&[*byte]).is_err() {
                                    return;
                                }
                            }
                        }
                        None => {
                            let _ = s.write_all(body);
                            std::thread::sleep(Duration::from_secs(120));
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    fn a_server_that_stops_sending_partway_is_given_up_on() {
        let port = answering(
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nxx",
            b"",
            None,
        );
        let started = std::time::Instant::now();
        let response = agent()
            .get(&format!("http://127.0.0.1:{port}/"))
            .call()
            .unwrap();
        let mut body = Vec::new();
        let read = response.into_body().into_reader().read_to_end(&mut body);
        assert!(
            read.is_err(),
            "the body never completes, so reading it must fail"
        );
        let waited = started.elapsed();
        assert!(
            waited >= IDLE && waited < IDLE + Duration::from_secs(10),
            "gave up after {waited:?}"
        );
    }

    #[test]
    fn a_body_that_keeps_arriving_is_not_cut_however_long_it_takes() {
        // Ten bytes two seconds apart: twenty seconds in all, four times the silence allowed.
        let port = answering(
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n",
            b"0123456789",
            Some(Duration::from_secs(2)),
        );
        let started = std::time::Instant::now();
        let response = agent()
            .get(&format!("http://127.0.0.1:{port}/"))
            .call()
            .unwrap();
        let mut body = String::new();
        response
            .into_body()
            .into_reader()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "0123456789");
        assert!(
            started.elapsed() > IDLE * 3,
            "the test must outlast the silence allowed"
        );
    }

    #[test]
    fn a_remote_that_stops_answering_ends_the_fetch_and_says_it_could_not_be_reached() {
        let port = answering(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nContent-Length: 4000\r\n\r\n001e# service=git-upload-pack\n",
            b"",
            None,
        );
        let dir = tempfile::tempdir().unwrap();
        let (repo, _) = recorded(dir.path(), b"x");
        install();
        let mut remote = repo
            .remote_anonymous(&format!("http://127.0.0.1:{port}/repo.git"))
            .unwrap();
        let started = std::time::Instant::now();
        let err = with_credentials(None, || remote.fetch(&[NOTES], None, None)).unwrap_err();
        assert!(
            started.elapsed() < IDLE + Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
        assert!(
            err.message().contains("could not reach the remote"),
            "{}",
            err.message()
        );
    }

    fn bare(root: &Path) {
        let repo = git2::Repository::init_bare(root.join("repo.git")).unwrap();
        repo.config()
            .unwrap()
            .set_bool("http.receivepack", true)
            .unwrap();
    }

    #[test]
    fn a_recorded_note_goes_to_the_remote_and_comes_back_on_another_machine() {
        install();
        let tmp = tempfile::tempdir().unwrap();
        bare(tmp.path());
        let Some(url) = serve(tmp.path().to_path_buf()) else {
            eprintln!("skipped: git is not installed");
            return;
        };

        let (here, id) = recorded(&tmp.path().join("here"), b"# Manual\n");
        with_credentials(right(), || {
            here.remote_anonymous(&url).unwrap().push(&[NOTES], None)
        })
        .expect("push");

        let there = git2::Repository::init(tmp.path().join("there")).unwrap();
        with_credentials(right(), || {
            there
                .remote_anonymous(&url)
                .unwrap()
                .fetch(&[NOTES], None, None)
        })
        .expect("fetch");
        let arrived = there.refname_to_id(crate::git_engine::NOTES_REF).unwrap();
        assert_eq!(arrived, id);
        let tree = there.find_commit(arrived).unwrap().tree().unwrap();
        let object = tree
            .get_name("manual.md")
            .unwrap()
            .to_object(&there)
            .unwrap();
        assert_eq!(object.as_blob().unwrap().content(), b"# Manual\n");
    }

    #[test]
    fn a_push_that_would_drop_the_remotes_history_does_not_land() {
        install();
        let tmp = tempfile::tempdir().unwrap();
        bare(tmp.path());
        let Some(url) = serve(tmp.path().to_path_buf()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let (first, first_id) = recorded(&tmp.path().join("first"), b"# One\n");
        with_credentials(right(), || {
            first.remote_anonymous(&url).unwrap().push(&[NOTES], None)
        })
        .expect("first push");

        // A second machine recorded its own, unrelated history and sends it without taking the
        // first one in: the remote must keep what it has.
        let (second, _) = recorded(&tmp.path().join("second"), b"# Two\n");
        let mut rejected: Option<String> = None;
        let outcome = {
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.push_update_reference(|name, status| {
                if let Some(s) = status {
                    rejected = Some(format!("{name}: {s}"));
                }
                Ok(())
            });
            let mut options = git2::PushOptions::new();
            options.remote_callbacks(callbacks);
            with_credentials(right(), || {
                second
                    .remote_anonymous(&url)
                    .unwrap()
                    .push(&[NOTES], Some(&mut options))
            })
        };
        let reason = match &outcome {
            Err(e) => e.message().to_string(),
            Ok(()) => rejected.clone().unwrap_or_default(),
        };
        // libgit2 refuses before sending ("… commits that are not present locally"); a server
        // that got the update anyway would answer with a non-fast-forward rejection.
        assert!(
            reason.contains("not present locally") || reason.contains("fast-forward"),
            "{reason}"
        );
        let remote = git2::Repository::open_bare(tmp.path().join("repo.git")).unwrap();
        assert_eq!(
            remote.refname_to_id(crate::git_engine::NOTES_REF).unwrap(),
            first_id
        );
    }

    #[test]
    fn wrong_credentials_are_refused_and_said_so() {
        install();
        let tmp = tempfile::tempdir().unwrap();
        bare(tmp.path());
        let Some(url) = serve(tmp.path().to_path_buf()) else {
            eprintln!("skipped: git is not installed");
            return;
        };
        let (here, _) = recorded(&tmp.path().join("here"), b"# Manual\n");
        let wrong = Some(Credentials {
            username: USER.into(),
            secret: "wrong".into(),
        });
        let err = with_credentials(wrong, || {
            here.remote_anonymous(&url).unwrap().push(&[NOTES], None)
        })
        .expect_err("a wrong token must fail");
        assert!(
            err.message().contains("refused these credentials"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn plain_http_is_only_for_this_machine() {
        assert!(check_url("https://example.com/team/notes.git").is_ok());
        assert!(check_url("http://127.0.0.1:8080/repo.git").is_ok());
        assert!(check_url("http://localhost/repo.git").is_ok());
        assert!(check_url("http://[::1]:9000/repo.git").is_ok());
        assert!(check_url("http://example.com/repo.git").is_err());
        assert!(check_url("http://127.0.0.1.example.com/repo.git").is_err());
        assert!(check_url("http://user@example.com/repo.git").is_err());
        assert!(check_url("ssh://example.com/repo.git").is_err());
    }

    #[test]
    fn credentials_last_only_for_the_operation() {
        with_credentials(right(), || {
            assert!(CURRENT.with(|c| c.borrow().is_some()));
            with_credentials(None, || assert!(CURRENT.with(|c| c.borrow().is_none())));
            assert!(CURRENT.with(|c| c.borrow().is_some()));
        });
        assert!(CURRENT.with(|c| c.borrow().is_none()));
    }
}

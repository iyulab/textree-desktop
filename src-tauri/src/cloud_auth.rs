//! In-app web-publish authentication (slice 2c): obtain a publish token via a browser OAuth
//! round-trip (RFC 8252 loopback + RFC 7636 PKCE) instead of manual token paste. The desktop only
//! ever receives the opaque publish token — never the cloud identity. The token is stored in
//! the OS keychain (publish_secret), the same entry the uploader reads.

use crate::cloud_publish::api_base;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const DEFAULT_APP_BASE: &str = "https://app.textree.me";
/// How long the loopback server waits for the browser redirect before giving up (cancel/timeout).
const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(180);
/// Bounds the code→token exchange request so a stalled server can't hang the flow.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

const SUCCESS_HTML: &str =
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>Textree</title></head>\
<body style=\"font-family:sans-serif;text-align:center;padding:3rem\"><h2>Connected to Textree</h2>\
<p>You can close this tab and return to the app.</p></body></html>";

/// The app (BFF) base — `TEXTREE_APP_BASE` override (dev/E2E builds only, `dev_env`) or the
/// production default.
pub fn app_base() -> String {
    crate::dev_env::var("TEXTREE_APP_BASE").unwrap_or_else(|| DEFAULT_APP_BASE.to_string())
}

/// 32 bytes of OS randomness, base64url-nopad (43 chars, all unreserved — RFC 7636 verifier set).
fn random_token() -> Result<String, String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub fn gen_verifier() -> Result<String, String> {
    random_token()
}
pub fn gen_state() -> Result<String, String> {
    random_token()
}

/// PKCE S256 challenge for a verifier: base64url-nopad(SHA256(verifier)). RFC 7636 §4.6. Pure.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Builds the browser authorize URL. state/challenge are base64url (URL-safe), so no extra encoding.
pub fn build_authorize_url(app_base: &str, port: u16, state: &str, challenge: &str) -> String {
    format!(
        "{}/connect/desktop?port={port}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        app_base.trim_end_matches('/'),
    )
}

/// Parses the loopback callback HTTP request line, returning (code, state). Pure.
/// e.g. "GET /callback?code=abc&state=xyz HTTP/1.1".
pub fn parse_callback_query(request_line: &str) -> Result<(String, String), String> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "malformed request".to_string())?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let (mut code, mut state) = (None, None);
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                _ => {}
            }
        }
    }
    match (code, state) {
        (Some(c), Some(s)) if !c.is_empty() && !s.is_empty() => Ok((c, s)),
        _ => Err("missing code or state in callback".to_string()),
    }
}

/// Parses the exchange success body `{ token }`. Pure.
pub fn parse_exchange_response(body: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        token: String,
    }
    let raw: Raw = serde_json::from_str(body).map_err(|e| format!("bad server response: {e}"))?;
    Ok(raw.token)
}

/// Maps a non-2xx exchange response to a user-facing message. Pure. The code/verifier never appear.
pub fn map_exchange_error(status: u16, _body: &str) -> String {
    match status {
        400 => "the sign-in expired or was invalid — try connecting again".to_string(),
        _ => format!("couldn't complete sign-in (HTTP {status})"),
    }
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Runs the loopback server: waits (bounded) for the browser redirect, validates `state`, replies
/// with a success page, and returns the authorization `code`. Consumes the listener.
///
/// Loop-tolerant (RFC 8252 practice): only a `/callback` request whose `state` matches ours
/// releases the code. Any other connection — a stray/speculative browser request, a favicon fetch,
/// a request that sends nothing, or a forged/non-matching `state` — gets a 404 and is ignored while
/// we keep waiting for the real redirect. The deadline bounds the total wait.
fn run_loopback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Switch to blocking + a read bound so a connection that sends nothing can't hang us.
                stream.set_nonblocking(false).map_err(|e| e.to_string())?;
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .map_err(|e| e.to_string())?;
                let mut request_line = String::new();
                let read_ok = {
                    let mut reader = BufReader::new(&stream);
                    reader.read_line(&mut request_line).is_ok()
                };
                match read_ok.then(|| parse_callback_query(&request_line)) {
                    Some(Ok((code, state))) if state == expected_state => {
                        write_response(&mut stream, "200 OK", SUCCESS_HTML);
                        return Ok(code);
                    }
                    // Not our callback (stray request, unreadable, or a non-matching/forged state):
                    // reply 404 and keep waiting for the real redirect until the deadline.
                    _ => write_response(&mut stream, "404 Not Found", "Not found."),
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "sign-in timed out — no response from the browser, try again".to_string(),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Exchanges the code + verifier for a publish token at the api (PKCE back-channel).
fn exchange_code(api_base: &str, code: &str, verifier: &str) -> Result<String, String> {
    let url = format!("{}/oauth/desktop/token", api_base.trim_end_matches('/'));
    // Error statuses come back as responses (not errors) so their body can explain the failure.
    match ureq::post(&url)
        .config()
        .timeout_global(Some(EXCHANGE_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .send_json(serde_json::json!({ "code": code, "codeVerifier": verifier }))
    {
        Ok(mut resp) if resp.status().is_success() => {
            let body = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| e.to_string())?;
            parse_exchange_response(&body)
        }
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            Err(map_exchange_error(status, &body))
        }
        Err(e) => Err(format!(
            "couldn't reach app.textree.me ({e}) — check your connection and try again"
        )),
    }
}

/// Full in-app connect flow: browser OAuth (loopback + PKCE) → publish token → OS keychain. The
/// frontend calls this when no token is stored; on success the caller can proceed to publish.
pub fn connect() -> Result<(), String> {
    let verifier = gen_verifier()?;
    let challenge = challenge_for(&verifier);
    let state = gen_state()?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("couldn't start local sign-in ({e}) — try again"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let url = build_authorize_url(&app_base(), port, &state, &challenge);
    tauri_plugin_opener::open_url(url, None::<&str>)
        .map_err(|e| format!("couldn't open the browser ({e})"))?;

    let code = run_loopback(listener, &state, LOOPBACK_TIMEOUT)?;
    let token = exchange_code(&api_base(), &code, &verifier)?;
    crate::publish_secret::set_token(&token)
}

/// Runs the in-app web-publish sign-in and stores the resulting token. Offloaded to a blocking
/// thread so the loopback wait (up to 3 min) never blocks the UI thread.
#[tauri::command]
pub async fn connect_publish() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(connect)
        .await
        .map_err(|e| format!("sign-in task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::Shutdown;

    #[test]
    fn challenge_for_matches_rfc7636_test_vector() {
        // RFC 7636 Appendix B.
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn gen_verifier_is_unique_urlsafe_43() {
        let a = gen_verifier().unwrap();
        let b = gen_verifier().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43); // 32 bytes base64url-nopad
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn build_authorize_url_has_all_pkce_params() {
        let url = build_authorize_url("https://app.test/", 54321, "st-8", "ch_9");
        assert_eq!(
            url,
            "https://app.test/connect/desktop?port=54321&state=st-8&code_challenge=ch_9&code_challenge_method=S256"
        );
    }

    #[test]
    fn parse_callback_query_extracts_code_and_state() {
        let (code, state) =
            parse_callback_query("GET /callback?code=abc123&state=xyz-9 HTTP/1.1").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz-9");
    }

    #[test]
    fn parse_callback_query_rejects_missing() {
        assert!(parse_callback_query("GET /callback?code=abc HTTP/1.1").is_err());
        assert!(parse_callback_query("GET /callback HTTP/1.1").is_err());
        assert!(parse_callback_query("garbage").is_err());
    }

    #[test]
    fn parse_exchange_response_extracts_token_and_rejects_garbage() {
        assert_eq!(
            parse_exchange_response(r#"{"token":"tk_abc"}"#).unwrap(),
            "tk_abc"
        );
        assert!(parse_exchange_response("not json").is_err());
    }

    #[test]
    fn map_exchange_error_explains_400_and_other() {
        assert!(map_exchange_error(400, "")
            .to_lowercase()
            .contains("try connecting again"));
        assert!(map_exchange_error(503, "").contains("503"));
    }

    #[test]
    fn run_loopback_returns_code_when_state_matches() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"GET /callback?code=thecode&state=goodstate HTTP/1.1\r\n\r\n")
                .unwrap();
        });
        let code = run_loopback(listener, "goodstate", Duration::from_secs(5)).unwrap();
        assert_eq!(code, "thecode");
    }

    #[test]
    fn run_loopback_skips_stray_and_wrong_state_then_accepts_callback() {
        // A stray request and a forged-state request must be ignored (404), and the real callback
        // that follows must still succeed — loop-tolerance + state binding in one.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for req in [
                "GET /favicon.ico HTTP/1.1\r\n\r\n",
                "GET /callback?code=x&state=WRONG HTTP/1.1\r\n\r\n",
                "GET /callback?code=thecode&state=goodstate HTTP/1.1\r\n\r\n",
            ] {
                if let Ok(mut s) = TcpStream::connect(addr) {
                    let _ = s.write_all(req.as_bytes());
                    // Drain the response so the server finishes this connection before the next.
                    let mut buf = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut s, &mut buf);
                }
            }
        });
        let code = run_loopback(listener, "goodstate", Duration::from_secs(5)).unwrap();
        assert_eq!(code, "thecode");
    }

    #[test]
    fn run_loopback_times_out_without_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let start = Instant::now();
        let err = run_loopback(listener, "st", Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn exchange_code_posts_and_returns_token() {
        // Fake api server: read the whole request, respond 200 with a token body. The request body
        // must be consumed before responding — closing a socket that still holds unread data is a
        // reset, not an orderly shutdown, and the client would race it and see a broken connection
        // instead of the response.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let mut content_length = 0usize;
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line
                    .strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
                {
                    content_length = v.trim().parse().unwrap();
                }
            }
            let mut request_body = vec![0u8; content_length];
            reader.read_exact(&mut request_body).unwrap();
            let body = r#"{"token":"tk_exchanged"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
            // Half-close so the peer sees end-of-response as a FIN rather than a reset.
            stream.shutdown(Shutdown::Write).unwrap();
        });
        let base = format!("http://{addr}");
        let token = exchange_code(&base, "thecode", "theverifier").unwrap();
        assert_eq!(token, "tk_exchanged");
        server.join().unwrap();
    }
}

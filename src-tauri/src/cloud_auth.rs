//! In-app web-publish sign-in: the person signs in at the identity provider in their own browser
//! (OAuth 2.0 for native apps, through `authway-native`), and the app trades the access token it
//! gets for an opaque publish token at the publishing API. Only that publish token is kept — in the
//! OS keychain, the entry the uploader reads. The provider's session is never stored: it lives in
//! memory for the one exchange and is revoked right after, so the app holds no account or session.

use crate::cloud_publish::api_base;
use authway_native::{Config, Error as SignInError, MemoryStore, NativeClient};
use std::time::Duration;

const DEFAULT_ISSUER: &str = "https://oauth.authway.in";
// TODO: confirm the client id once the production client is registered, before the next release.
const DEFAULT_CLIENT_ID: &str = "textree-desktop";
/// How long the loopback server waits for the browser redirect before giving up (cancel/timeout).
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(180);
/// Bounds the token exchange request so a stalled server can't hang the flow.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Textree</title></head>\
<body style=\"font-family:sans-serif;text-align:center;padding:3rem\"><h2>Connected to Textree</h2>\
<p>You can close this tab and return to the app.</p></body></html>";

const FAILURE_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Textree</title></head>\
<body style=\"font-family:sans-serif;text-align:center;padding:3rem\"><h2>Sign-in did not complete</h2>\
<p>Return to Textree and try again.</p></body></html>";

/// The identity provider — `TEXTREE_AUTH_ISSUER` override (dev/E2E builds only, `dev_env`) or the
/// production default.
pub fn issuer() -> String {
    crate::dev_env::var("TEXTREE_AUTH_ISSUER").unwrap_or_else(|| DEFAULT_ISSUER.to_string())
}

/// The app's registered client id — `TEXTREE_AUTH_CLIENT_ID` override (dev/E2E builds only) or the
/// production default.
pub fn client_id() -> String {
    crate::dev_env::var("TEXTREE_AUTH_CLIENT_ID").unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

fn client_for(issuer: &str, client_id: &str) -> NativeClient {
    // `openid` only: the person is identified by the access token's subject, and no refresh token
    // is asked for — nothing here outlives the one exchange.
    let config = Config::new(issuer, client_id).scopes(["openid"]).pages(SUCCESS_HTML, FAILURE_HTML);
    NativeClient::new(config, MemoryStore::default())
}

/// What the person is told when the sign-in did not complete. Pure — the provider's own error text
/// (which can carry request details) is not passed on.
pub fn sign_in_error(e: &SignInError) -> String {
    match e {
        SignInError::TimedOut => "sign-in timed out — try connecting again".to_string(),
        SignInError::Cancelled => "sign-in was cancelled".to_string(),
        SignInError::Denied { .. } => "sign-in was declined — try connecting again".to_string(),
        SignInError::Http(_) | SignInError::Discovery(_) => {
            "couldn't reach the sign-in service — check your connection and try again".to_string()
        }
        SignInError::Loopback(_) => "couldn't start local sign-in — try again".to_string(),
        _ => "couldn't complete sign-in — try connecting again".to_string(),
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

/// Maps a non-2xx exchange response to a user-facing message. Pure. The access token never appears.
pub fn map_exchange_error(status: u16, _body: &str) -> String {
    match status {
        401 => "the sign-in expired or was invalid — try connecting again".to_string(),
        _ => format!("couldn't complete sign-in (HTTP {status})"),
    }
}

/// Trades the provider's access token for a publish token at the publishing API.
fn exchange_token(api_base: &str, access_token: &str) -> Result<String, String> {
    let url = format!("{}/tokens/desktop", api_base.trim_end_matches('/'));
    // Error statuses come back as responses (not errors) so their status can explain the failure.
    match ureq::post(&url)
        .header("Authorization", &format!("Bearer {access_token}"))
        .config()
        .timeout_global(Some(EXCHANGE_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .send_empty()
    {
        Ok(mut resp) if resp.status().is_success() => {
            let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
            parse_exchange_response(&body)
        }
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            Err(map_exchange_error(status, &body))
        }
        Err(e) => Err(format!("couldn't reach the publishing service ({e}) — check your connection and try again")),
    }
}

/// Full in-app connect flow: browser sign-in → access token → publish token → OS keychain. The
/// frontend calls this when no token is stored; on success the caller can proceed to publish. A
/// failed sign-in changes nothing — the stored token is replaced only once a new one is in hand.
pub fn connect() -> Result<(), String> {
    let client = client_for(&issuer(), &client_id());
    let pending = client.begin_sign_in().map_err(|e| sign_in_error(&e))?;
    tauri_plugin_opener::open_url(pending.url(), None::<&str>).map_err(|e| format!("couldn't open the browser ({e})"))?;
    pending.wait(SIGN_IN_TIMEOUT).map_err(|e| sign_in_error(&e))?;

    let access_token = client
        .access_token()
        .map_err(|e| sign_in_error(&e))?
        .ok_or_else(|| "couldn't complete sign-in — try connecting again".to_string())?;
    let token = exchange_token(&api_base(), &access_token);
    // The provider's session is not kept: revoke it whether or not the exchange worked. Best
    // effort — the access token is short-lived and was only ever held in memory.
    let _ = client.sign_out();
    crate::publish_secret::set_token(&token?)
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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener};

    #[test]
    fn parse_exchange_response_extracts_token_and_rejects_garbage() {
        assert_eq!(parse_exchange_response(r#"{"token":"tk_abc"}"#).unwrap(), "tk_abc");
        assert!(parse_exchange_response("nope").is_err());
        assert!(parse_exchange_response(r#"{"nottoken":1}"#).is_err());
    }

    #[test]
    fn map_exchange_error_explains_401_and_other() {
        assert!(map_exchange_error(401, "").contains("expired or was invalid"));
        assert!(map_exchange_error(500, "x").contains("500"));
    }

    #[test]
    fn sign_in_errors_read_as_sentences_without_the_providers_text() {
        let denied = SignInError::Denied { error: "access_denied".into(), description: Some("secret detail".into()) };
        let msg = sign_in_error(&denied);
        assert!(msg.contains("declined"));
        assert!(!msg.contains("secret detail") && !msg.contains("access_denied"));
        assert!(sign_in_error(&SignInError::TimedOut).contains("timed out"));
        assert!(sign_in_error(&SignInError::Cancelled).contains("cancelled"));
        assert!(sign_in_error(&SignInError::Http("tcp reset".into())).contains("couldn't reach"));
    }

    /// One-shot fake API: answers a single request and hands back what the client sent.
    fn fake_api(status_line: &'static str, body: &'static str) -> (String, std::thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut authorization = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("authorization:") {
                    authorization = line[line.len() - v.len()..].trim().to_string();
                }
                if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap();
                }
            }
            // Consume any request body before answering — closing a socket that still holds unread
            // data is a reset, and the client would see a broken connection instead of the response.
            let mut sink = vec![0u8; content_length];
            reader.read_exact(&mut sink).unwrap();
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            (request_line, authorization)
        });
        (format!("http://{addr}"), server)
    }

    #[test]
    fn exchange_token_posts_the_access_token_as_a_bearer_and_returns_the_publish_token() {
        let (base, server) = fake_api("200 OK", r#"{"token":"tk_exchanged"}"#);
        let token = exchange_token(&base, "the-access-token").unwrap();
        assert_eq!(token, "tk_exchanged");
        let (request_line, authorization) = server.join().unwrap();
        assert!(request_line.starts_with("POST /tokens/desktop "), "{request_line}");
        assert_eq!(authorization, "Bearer the-access-token");
    }

    #[test]
    fn exchange_token_turns_a_refusal_into_a_message_without_the_token() {
        let (base, server) = fake_api("401 Unauthorized", "");
        let err = exchange_token(&base, "the-access-token").unwrap_err();
        assert!(err.contains("expired or was invalid"), "{err}");
        assert!(!err.contains("the-access-token"));
        server.join().unwrap();
    }
}

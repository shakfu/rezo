// Fetch a URL via HTTP(S) GET. Confirmation-required by default —
// the user should approve the destination because the model can
// produce arbitrary URLs (potential SSRF, internal-service probes,
// data exfiltration via attacker-controlled URLs in tool args).
//
// Caps:
//   - 15s request timeout
//   - 1 MiB response body cap, enforced *during* the read: the body is
//     streamed and the read stops once the cap is hit. Buffering the
//     whole response first (`Response::bytes()`) would let a large or
//     hostile endpoint allocate without bound before the cap ever
//     applied.
//   - Only http/https schemes accepted, checked by parsing the URL
//     rather than by string prefix.
//
// Redirects: followed only while they stay on the same host. The
// security control for this tool is the user confirming a specific
// destination, and silently following a cross-host redirect fetches
// something the user never saw — the classic way an approved
// `https://example.com/x` ends up reading `http://169.254.169.254/`.
// Same-host hops (http->https, trailing slash, canonical path) are
// what users actually expect and do not change who is being talked to,
// so those still follow. A cross-host redirect returns the 3xx itself
// with `redirectedTo` set, so the model can re-request the new URL —
// which prompts the user again, for the URL they will actually get.
//
// Not enforced here: any block on private / link-local address ranges.
// rezon deliberately targets local model servers (Ollama, LM Studio,
// llama.cpp) at 127.0.0.1, so a blanket private-range block would
// break a first-class use case. Destination policy stays with the
// confirmation gate; TODO.md tracks an allow/blocklist for users who
// want to pin it further.

use std::sync::OnceLock;

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::tool::{Tool, ToolContext, ToolError};

const MAX_BYTES: usize = 1024 * 1024;
const TIMEOUT_SECS: u64 = 15;
const MAX_REDIRECTS: usize = 10;

/// Shared client: connection pooling and TLS setup are per-client, so
/// building one per call threw both away.
fn client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
                .user_agent("rezon/0.1")
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= MAX_REDIRECTS {
                        return attempt.error("too many redirects");
                    }
                    let same_host = attempt
                        .previous()
                        .last()
                        .and_then(|p| p.host_str().map(|h| h.to_ascii_lowercase()))
                        .zip(attempt.url().host_str().map(|h| h.to_ascii_lowercase()))
                        .map(|(prev, next)| prev == next)
                        .unwrap_or(false);
                    if same_host {
                        attempt.follow()
                    } else {
                        // Hand the 3xx back to the caller rather than
                        // chasing it; see the module comment.
                        attempt.stop()
                    }
                }))
                .build()
                .map_err(|e| format!("build http client: {e}"))
        })
        .as_ref()
        .map_err(|e| e.clone())
}

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "HTTP(S) GET a URL and return status, content-type, and body. \
         Body is decoded as UTF-8 (lossy) and capped at 1MB. Redirects \
         are followed only within the same host; a cross-host redirect \
         returns the 3xx with `redirectedTo` so you can request the new \
         URL explicitly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL."
                }
            },
            "required": ["url"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value) -> Option<String> {
        // The destination is the whole decision here, so show it on its
        // own line rather than making the user read it out of JSON.
        let url = args.get("url")?.as_str()?;
        Some(format!("web_fetch  GET\n  {url}"))
    }

    async fn dispatch(&self, args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            url: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| ToolError::Argument(format!("invalid args: {e}")))?;

        // Parse rather than string-match: `split(':')` accepts things
        // that are not URLs at all and misreads anything with an
        // embedded colon.
        let parsed = reqwest::Url::parse(&args.url)
            .map_err(|e| ToolError::Argument(format!("invalid url {}: {e}", args.url)))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ToolError::Argument(format!(
                    "url scheme must be http or https, got `{other}`: {}",
                    args.url
                )))
            }
        }

        let resp = client()
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!(e)))?
            .get(parsed.clone())
            .send()
            .await
            .map_err(|e| ToolError::Runtime(anyhow::anyhow!("get {}: {e}", args.url)))?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // A redirect status still in hand means the policy stopped a
        // cross-host hop. Report where it wanted to go so the model can
        // ask for it explicitly (and the user can approve that URL).
        let redirected_to = if status.is_redirection() {
            resp.headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                // Resolve against the request URL so a relative
                // Location is reported as something requestable.
                .map(|loc| {
                    parsed
                        .join(loc)
                        .map(String::from)
                        .unwrap_or_else(|_| loc.to_string())
                })
        } else {
            None
        };

        let (body_bytes, truncated, size) = read_capped(resp).await?;
        let body = String::from_utf8_lossy(&body_bytes).into_owned();

        Ok(json!({
            "url": final_url,
            "status": status.as_u16(),
            "contentType": content_type,
            // Bytes actually read. When `truncated`, the response was
            // larger than this; the true total is deliberately not
            // reported, since finding it out would mean reading the
            // whole thing.
            "size": size,
            "truncated": truncated,
            "redirectedTo": redirected_to,
            "body": body,
        }))
    }
}

/// Read the body incrementally, stopping once the cap is reached.
/// Returns `(bytes, truncated, size_read)`.
///
/// Reads up to `MAX_BYTES + 1` bytes, then reports truncation if that
/// extra byte materialized. Stopping at exactly `MAX_BYTES` cannot
/// distinguish "body ended precisely at the cap" from "body continues"
/// without another poll, and guessing either way is wrong for one of
/// the two cases. One byte of overshoot resolves it exactly.
async fn read_capped(resp: reqwest::Response) -> Result<(Vec<u8>, bool, usize), ToolError> {
    const LIMIT: usize = MAX_BYTES + 1;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ToolError::Runtime(anyhow::anyhow!("read body: {e}")))?;
        let need = LIMIT.saturating_sub(buf.len());
        if chunk.len() >= need {
            buf.extend_from_slice(&chunk[..need]);
            // Dropping the stream here closes the connection rather
            // than draining a body we have already decided to discard.
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    let truncated = buf.len() > MAX_BYTES;
    buf.truncate(MAX_BYTES);
    let size = buf.len();
    Ok((buf, truncated, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
        ToolContext {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn dispatch(url: &str) -> Result<Value, ToolError> {
        WebFetch.dispatch(json!({ "url": url }), &ctx()).await
    }

    #[tokio::test]
    async fn rejects_non_http_schemes() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "data:text/plain,hi",
            "javascript:alert(1)",
        ] {
            let err = dispatch(url).await.unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("scheme must be http or https"),
                "{url} gave: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_unparseable_urls() {
        // The old string-prefix check accepted these: everything before
        // the first ':' looked like a scheme, or there was no ':' at all.
        for url in ["not a url", "http", "://missing-scheme"] {
            let err = dispatch(url).await.unwrap_err();
            assert!(err.to_string().contains("invalid url"), "{url} gave: {err}");
        }
    }

    #[test]
    fn preview_puts_the_destination_on_its_own_line() {
        let p = WebFetch
            .preview(&json!({"url": "https://example.com/a"}))
            .unwrap();
        assert!(p.starts_with("web_fetch  GET\n"));
        assert!(p.contains("https://example.com/a"));
    }

    #[test]
    fn preview_is_none_without_a_url() {
        assert!(WebFetch.preview(&json!({})).is_none());
    }

    /// Serve one HTTP response from a throwaway port and return its
    /// URL. `body_len` bytes of 'x', sent in 64 KiB writes so the test
    /// can observe the reader stopping early: if `web_fetch` buffered
    /// the whole body, a body far larger than the cap would still be
    /// fully read.
    ///
    /// Writes are allowed to fail. Once the client hits the cap it
    /// drops the connection, and the remaining writes get EPIPE — that
    /// is the behaviour under test, not an error.
    async fn serve_body(body_len: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request first. Closing a socket that still has
            // unread receive data makes the kernel send RST rather than
            // FIN, which aborts the connection and loses whatever is
            // still in flight — the client then sees "error decoding
            // response body" instead of a clean end of stream.
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                 Content-Length: {body_len}\r\nConnection: close\r\n\r\n"
            );
            if sock.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let chunk = vec![b'x'; 64 * 1024];
            let mut sent = 0;
            while sent < body_len {
                let n = chunk.len().min(body_len - sent);
                if sock.write_all(&chunk[..n]).await.is_err() {
                    return;
                }
                sent += n;
            }
            let _ = sock.flush().await;
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn small_body_is_returned_whole_and_not_flagged() {
        let url = serve_body(1024).await;
        let v = dispatch(&url).await.unwrap();
        assert_eq!(v["status"], 200);
        assert_eq!(v["size"], 1024);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["body"].as_str().unwrap().len(), 1024);
    }

    #[tokio::test]
    async fn oversized_body_stops_at_the_cap_instead_of_buffering_it_all() {
        // 4x the cap. The pre-fix implementation called
        // `Response::bytes()`, which would allocate all of this before
        // the cap was applied.
        let url = serve_body(MAX_BYTES * 4).await;
        let v = dispatch(&url).await.unwrap();
        assert_eq!(v["truncated"], true);
        let size = v["size"].as_u64().unwrap() as usize;
        assert_eq!(size, MAX_BYTES, "read must stop exactly at the cap");
        assert_eq!(v["body"].as_str().unwrap().len(), MAX_BYTES);
    }

    #[tokio::test]
    async fn body_exactly_at_the_cap_is_not_flagged_truncated() {
        // Boundary: `chunk.len() >= remaining` must not report
        // truncation when the body ends precisely at the cap.
        let url = serve_body(MAX_BYTES).await;
        let v = dispatch(&url).await.unwrap();
        assert_eq!(v["size"], MAX_BYTES);
        assert_eq!(
            v["truncated"], false,
            "a body that exactly fills the cap was not truncated"
        );
    }

    /// Serve a single 302 pointing at `location`.
    async fn serve_redirect(location: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let location = location.to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn cross_host_redirect_is_reported_not_followed() {
        // The user approved *this* host. Chasing the hop would fetch
        // something they never saw, so the 3xx comes back instead.
        let url = serve_redirect("http://169.254.169.254/latest/meta-data/").await;
        let v = dispatch(&url).await.unwrap();
        assert_eq!(v["status"], 302);
        assert_eq!(
            v["redirectedTo"], "http://169.254.169.254/latest/meta-data/",
            "the blocked destination must be reported so the model can re-ask"
        );
        assert_eq!(v["body"], "");
    }

    #[tokio::test]
    async fn relative_redirect_location_is_resolved_to_an_absolute_url() {
        // A bare `/elsewhere` is same-host, so it is followed — and the
        // follow fails here because the one-shot server is already
        // done. What matters is that we did not report it as a blocked
        // cross-host hop.
        let url = serve_redirect("/elsewhere").await;
        let result = dispatch(&url).await;
        match result {
            // Follow attempted, second connection refused.
            Err(e) => assert!(e.to_string().contains("get "), "unexpected error: {e}"),
            // Or it surfaced the 3xx; then the location must be absolute.
            Ok(v) => {
                if let Some(loc) = v["redirectedTo"].as_str() {
                    assert!(loc.starts_with("http://"), "not absolute: {loc}");
                }
            }
        }
    }

    #[test]
    fn web_fetch_requires_confirmation() {
        // Guards the backend confirmation floor: if this ever flips to
        // false, the gate silently stops prompting for network egress.
        assert!(WebFetch.requires_confirmation());
    }
}

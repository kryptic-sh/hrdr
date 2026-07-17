use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::oneshot;

use hrdr_llm::SseDecoder;

use crate::truncate;

use super::types::Pending;
use super::{HttpTransport, PROTOCOL_VERSION, SseTransport, StdioTransport, response_id};

/// Shared plumbing for stdio + SSE: register an id in `pending`, execute
/// `send_fn` (which should fire off the request), then race `rx` against
/// `timeout`. On send failure or timeout the id is cleaned up.
pub(crate) async fn request_via_pending<F, Fut>(
    pending: &Pending,
    id: u64,
    timeout: Duration,
    send_fn: F,
) -> Result<Value>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (tx, rx) = oneshot::channel();
    {
        let mut p = pending.lock().await;
        p.insert(id, tx);
    }
    if let Err(e) = send_fn().await {
        pending.lock().await.remove(&id);
        return Err(anyhow!("send failed: {e}"));
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(msg)) => Ok(msg),
        Ok(Err(_)) => bail!("connection closed"),
        Err(_) => {
            pending.lock().await.remove(&id);
            bail!("request timed out")
        }
    }
}

/// stdio: register the id, write the line, await the raw response message.
pub(crate) async fn stdio_request(
    t: &StdioTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let msg = req.to_string();
    request_via_pending(&t.pending, id, timeout, || async move {
        t.stdin_tx
            .send(msg)
            .map_err(|_| anyhow!("server is not running"))
    })
    .await
}

/// Streamable HTTP: POST the request; parse the JSON or SSE response for `id`.
pub(crate) async fn http_request(
    t: &HttpTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let resp = tokio::time::timeout(timeout, http_post(t, &req).send())
        .await
        .map_err(|_| anyhow!("timed out"))?
        .context("request failed")?;
    // Capture the session id (returned on `initialize`) for later requests.
    if let Some(sid) = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        *t.session.lock().unwrap() = Some(sid.to_string());
    }
    let status = resp.status();
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("text/event-stream"));
    let body = read_body_capped(resp).await?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", truncate(body.trim(), 500));
    }
    if is_sse {
        parse_sse_for_id(&body, id)
    } else {
        serde_json::from_str(&body).with_context(|| format!("decoding response: {body}"))
    }
}

/// Read an HTTP response body under both a size cap
/// ([`super::MAX_MCP_MESSAGE_BYTES`]) and a wall-clock cap
/// ([`super::MAX_BODY_READ_TIME`]) — `resp.text().await` alone has neither, so
/// a hostile or hung MCP server could stream unbounded data, or a connection
/// that never finishes, into this process. MCP responses are small JSON-RPC
/// envelopes, so both limits are generous.
async fn read_body_capped(resp: reqwest::Response) -> Result<String> {
    let cap = super::MAX_MCP_MESSAGE_BYTES;
    let read = async move {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading response body")?;
            push_capped_or_err(&mut buf, &chunk, cap)?;
        }
        Ok::<String, anyhow::Error>(String::from_utf8_lossy(&buf).into_owned())
    };
    match tokio::time::timeout(super::MAX_BODY_READ_TIME, read).await {
        Ok(result) => result,
        Err(_) => bail!("timed out reading response body"),
    }
}

/// Append `chunk` to `buf`, refusing once the total would exceed `cap` — the
/// size-cap logic behind [`read_body_capped`], split out so it's unit-testable
/// without a live connection.
fn push_capped_or_err(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> Result<()> {
    if buf.len() + chunk.len() > cap {
        bail!("response body exceeded the {cap}-byte cap");
    }
    buf.extend_from_slice(chunk);
    Ok(())
}

/// Fire-and-forget HTTP POST (for notifications).
pub(crate) async fn http_send(t: &HttpTransport, msg: &Value) -> Result<()> {
    http_post(t, msg).send().await.context("request failed")?;
    Ok(())
}

/// Legacy HTTP+SSE: POST the request to the endpoint; the response arrives back
/// on the persistent SSE stream and is delivered via `pending`.
pub(crate) async fn sse_request(
    t: &SseTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let post_url = t
        .post_url
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("no endpoint"))?;
    request_via_pending(&t.pending, id, timeout, || async {
        let resp = t
            .http
            .post(&post_url)
            .headers(t.headers.clone())
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e).context("request failed"))?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        Ok(())
    })
    .await
}

/// Build a [`HeaderMap`] from `(name, value)` pairs (config auth headers).
pub(crate) fn build_headers(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid MCP header name '{k}'"))?;
        let val = HeaderValue::from_str(v)
            .with_context(|| format!("invalid MCP header value for '{k}'"))?;
        map.insert(name, val);
    }
    Ok(map)
}

/// Build a POST request with the MCP headers + session id.
pub(crate) fn http_post(t: &HttpTransport, body: &Value) -> reqwest::RequestBuilder {
    let mut req = t
        .http
        .post(&t.url)
        .headers(t.headers.clone())
        .header(ACCEPT, "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .json(body);
    if let Some(sid) = t.session.lock().unwrap().clone() {
        req = req.header("Mcp-Session-Id", sid);
    }
    req
}

/// Find the JSON-RPC *response* message for `id` in an SSE stream body.
///
/// Uses [`SseDecoder`] for correct blank-line-terminated event grouping and
/// multi-line `data:` folding (mirrors the Streamable-HTTP inline SSE path).
/// A trailing `\n\n` is pushed after the body to flush any event that was not
/// terminated in the buffer (some servers omit the final blank line).
///
/// Matching goes through [`response_id`], not a raw `id` comparison: a
/// server-initiated request/notification (has a `method` field) must never
/// be mistaken for the response to our own request, even if its `id`
/// collides — id spaces are per-sender, so collisions are expected, not
/// exceptional. A response whose `id` was echoed back as a string is also
/// accepted, as long as it parses as this `u64`.
pub(crate) fn parse_sse_for_id(body: &str, id: u64) -> Result<Value> {
    let mut dec = SseDecoder::new();
    dec.push(body.as_bytes());
    if dec.overflowed() {
        bail!("SSE response body exceeded maximum size (32 MiB)");
    }
    // Force-flush a trailing event that isn't blank-line-terminated.
    dec.push(b"\n\n");
    if dec.overflowed() {
        bail!("SSE response body exceeded maximum size (32 MiB)");
    }
    for ev in dec.drain() {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.data)
            && response_id(&v) == Some(id)
        {
            return Ok(v);
        }
    }
    bail!("no response for request {id} in the SSE stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size-cap guard behind an MCP HTTP response read: within the cap,
    /// chunks accumulate; the chunk that would push the total past `cap` is
    /// refused with a clear error rather than silently buffered.
    #[test]
    fn push_capped_or_err_bounds_response_body_size() {
        let mut buf = Vec::new();
        assert!(push_capped_or_err(&mut buf, b"hello", 10).is_ok());
        assert!(push_capped_or_err(&mut buf, b"world", 10).is_ok());
        assert_eq!(buf, b"helloworld");
        let err = push_capped_or_err(&mut buf, b"!", 10).unwrap_err();
        assert!(err.to_string().contains("exceeded"), "{err}");
        // The buffer isn't left half-mutated by the refused push.
        assert_eq!(buf, b"helloworld");
    }

    // Regression for a MAJOR bug: id spaces are per-sender, so a
    // server-initiated request can legitimately reuse an id we're waiting on
    // for our own request. `parse_sse_for_id` must not hand that request back
    // as if it were the response — it has to keep scanning for the real one.
    #[test]
    fn parse_sse_for_id_ignores_server_initiated_message_with_colliding_id() {
        let body = "\
event: message
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{}}

event: message
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}

";
        let v = parse_sse_for_id(body, 1).unwrap();
        assert_eq!(v["result"]["ok"], true, "{v}");
    }

    // A server that echoes the id back as a JSON string, not a number, must
    // still be matched (id "1" answers request 1).
    #[test]
    fn parse_sse_for_id_accepts_a_string_id() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":\"3\",\"result\":{\"ok\":true}}\n\n";
        let v = parse_sse_for_id(body, 3).unwrap();
        assert_eq!(v["result"]["ok"], true, "{v}");
    }
}

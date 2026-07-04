//! Minimal MCP (Model Context Protocol) client.
//!
//! Connects to an MCP server, runs the `initialize` handshake, discovers its
//! tools (`tools/list`), and exposes each as a [`Tool`] the model can call
//! (`tools/call`). Two transports:
//!
//! - **stdio** — spawn a process; newline-delimited JSON-RPC 2.0 on its
//!   stdin/stdout. One background reader routes responses to pending requests by
//!   id, so calls can be concurrent.
//! - **Streamable HTTP** — POST each JSON-RPC request to a single URL; the
//!   response is either `application/json` (one message) or an SSE stream
//!   (`text/event-stream`). The server's `Mcp-Session-Id` header is echoed back
//!   on later requests.
//!
//! Scope (v1): tools only (no resources/prompts) and no server-initiated
//! requests (we don't open the optional GET stream).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

use crate::{Tool, ToolContext, truncate};

/// MCP protocol version we advertise. Servers negotiate down if needed; the
/// `tools/*` methods are stable across recent revisions.
const PROTOCOL_VERSION: &str = "2025-06-18";
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Outstanding stdio requests, keyed by id → a sink for the raw response message.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A live connection to one MCP server.
pub struct McpClient {
    server: String,
    next_id: AtomicU64,
    transport: Transport,
}

enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

/// stdio transport: a spawned child + a writer channel + the id→response map.
/// Dropping it kills the child (`kill_on_drop`).
struct StdioTransport {
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    pending: Pending,
    _child: Child,
}

/// Streamable-HTTP transport: POST to `url`, carrying `headers` (auth) and the
/// server-assigned session id once known.
struct HttpTransport {
    http: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session: StdMutex<Option<String>>,
}

/// A tool advertised by an MCP server's `tools/list`.
struct Discovered {
    name: String,
    description: String,
    schema: Value,
    read_only: bool,
}

impl McpClient {
    /// Spawn `command args…` (with extra `env`) and connect over stdio.
    pub async fn connect_stdio(
        server: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<(Arc<Self>, Vec<Arc<dyn Tool>>)> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server '{server}' ({command})"))?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout pipe"))?;

        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue; // skip non-JSON (some servers emit banners)
                    };
                    if let Some(id) = msg.get("id").and_then(Value::as_u64)
                        && let Some(tx) = pending.lock().await.remove(&id)
                    {
                        let _ = tx.send(msg);
                    }
                    // notifications + server-initiated requests are ignored (v1).
                }
                pending.lock().await.clear(); // closing drops senders → callers error
            });
        }

        let client = Arc::new(Self {
            server: server.to_string(),
            next_id: AtomicU64::new(1),
            transport: Transport::Stdio(StdioTransport {
                stdin_tx,
                pending,
                _child: child,
            }),
        });
        let tools = client.clone().handshake_and_list().await?;
        Ok((client, tools))
    }

    /// Connect over Streamable HTTP to `url`, sending `headers` (e.g. auth) with
    /// every request.
    pub async fn connect_http(
        server: &str,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<(Arc<Self>, Vec<Arc<dyn Tool>>)> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .with_context(|| format!("invalid MCP header name '{k}'"))?;
            let val = HeaderValue::from_str(v)
                .with_context(|| format!("invalid MCP header value for '{k}'"))?;
            map.insert(name, val);
        }
        let client = Arc::new(Self {
            server: server.to_string(),
            next_id: AtomicU64::new(1),
            transport: Transport::Http(HttpTransport {
                http: reqwest::Client::new(),
                url: url.to_string(),
                headers: map,
                session: StdMutex::new(None),
            }),
        });
        let tools = client.clone().handshake_and_list().await?;
        Ok((client, tools))
    }

    /// Run `initialize` + `tools/list`, wrapping each discovered tool.
    async fn handshake_and_list(self: Arc<Self>) -> Result<Vec<Arc<dyn Tool>>> {
        self.initialize().await?;
        let discovered = self.list_tools().await?;
        let server = self.server.clone();
        Ok(discovered
            .into_iter()
            .map(|d| {
                let exposed = sanitize_tool_name(&format!("{server}_{}", d.name));
                Arc::new(McpTool {
                    client: self.clone(),
                    // Leaked once at startup (bounded): the registry keys on
                    // `&'static str`, and these tools live for the process.
                    exposed_name: Box::leak(exposed.into_boxed_str()),
                    description: Box::leak(d.description.into_boxed_str()),
                    schema: d.schema,
                    remote_name: d.name,
                    read_only: d.read_only,
                }) as Arc<dyn Tool>
            })
            .collect())
    }

    /// Send a JSON-RPC request over whichever transport, and extract its result.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let msg = match &self.transport {
            Transport::Stdio(t) => stdio_request(t, id, req, timeout).await,
            Transport::Http(t) => http_request(t, id, req, timeout).await,
        }
        .with_context(|| format!("MCP '{}' {method}", self.server))?;
        if let Some(err) = msg.get("error") {
            bail!("MCP '{}' {method}: {}", self.server, rpc_error_message(err));
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a JSON-RPC notification (best-effort; no response expected).
    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stdio(t) => {
                let _ = t.stdin_tx.send(msg.to_string());
            }
            Transport::Http(t) => {
                let _ = http_send(t, &msg).await;
            }
        }
    }

    async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "hrdr", "version": env!("CARGO_PKG_VERSION") },
        });
        self.request("initialize", params, HANDSHAKE_TIMEOUT)
            .await?;
        self.notify("notifications/initialized", json!({})).await;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Discovered>> {
        let res = self
            .request("tools/list", json!({}), HANDSHAKE_TIMEOUT)
            .await?;
        let mut out = Vec::new();
        for t in res
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = t.get("name").and_then(Value::as_str) else {
                continue;
            };
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            let read_only = t
                .get("annotations")
                .and_then(|a| a.get("readOnlyHint"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            out.push(Discovered {
                name: name.to_string(),
                description,
                schema,
                read_only,
            });
        }
        Ok(out)
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        let res = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": args }),
                CALL_TIMEOUT,
            )
            .await?;
        let text = extract_content_text(&res);
        if res.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            bail!(
                "{}",
                if text.is_empty() {
                    "tool reported an error".to_string()
                } else {
                    text
                }
            );
        }
        Ok(text)
    }
}

/// stdio: register the id, write the line, await the raw response message.
async fn stdio_request(
    t: &StdioTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let (tx, rx) = oneshot::channel();
    t.pending.lock().await.insert(id, tx);
    t.stdin_tx
        .send(req.to_string())
        .map_err(|_| anyhow!("server is not running"))?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(msg)) => Ok(msg),
        Ok(Err(_)) => bail!("connection closed"),
        Err(_) => {
            t.pending.lock().await.remove(&id);
            bail!("timed out")
        }
    }
}

/// Streamable HTTP: POST the request; parse the JSON or SSE response for `id`.
async fn http_request(t: &HttpTransport, id: u64, req: Value, timeout: Duration) -> Result<Value> {
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
    let body = resp.text().await.context("reading response")?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", truncate(body.trim(), 500));
    }
    if is_sse {
        parse_sse_for_id(&body, id)
    } else {
        serde_json::from_str(&body).with_context(|| format!("decoding response: {body}"))
    }
}

/// Fire-and-forget HTTP POST (for notifications).
async fn http_send(t: &HttpTransport, msg: &Value) -> Result<()> {
    http_post(t, msg).send().await.context("request failed")?;
    Ok(())
}

/// Build a POST request with the MCP headers + session id.
fn http_post(t: &HttpTransport, body: &Value) -> reqwest::RequestBuilder {
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

/// Find the JSON-RPC message with `id` in an SSE stream body. Each event's
/// `data:` payload is one JSON-RPC message.
fn parse_sse_for_id(body: &str, id: u64) -> Result<Value> {
    let mut data = String::new();
    let check = |data: &str| -> Option<Value> {
        serde_json::from_str::<Value>(data)
            .ok()
            .filter(|v| v.get("id").and_then(Value::as_u64) == Some(id))
    };
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() && !data.is_empty() {
            if let Some(v) = check(&data) {
                return Ok(v);
            }
            data.clear();
        }
    }
    if !data.is_empty()
        && let Some(v) = check(&data)
    {
        return Ok(v);
    }
    bail!("no response for request {id} in the SSE stream")
}

/// One MCP-server tool, exposed to the model as a native [`Tool`].
struct McpTool {
    client: Arc<McpClient>,
    exposed_name: &'static str,
    description: &'static str,
    schema: Value,
    remote_name: String,
    read_only: bool,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.exposed_name
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let out = self.client.call_tool(&self.remote_name, args).await?;
        Ok(truncate(&out, ctx.max_output))
    }
}

/// Flatten an MCP tool result's `content` array into text (`type:"text"` parts),
/// noting any non-text parts the model can't see inline.
fn extract_content_text(result: &Value) -> String {
    let Some(parts) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            Some(other) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{other} content omitted]"));
            }
            None => {}
        }
    }
    out
}

/// Human-readable message from a JSON-RPC `error` object.
fn rpc_error_message(err: &Value) -> String {
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    match err.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{msg} (code {code})"),
        None => msg.to_string(),
    }
}

/// Reduce a namespaced tool name to a valid OpenAI function name
/// (`[a-zA-Z0-9_-]`), collapsing anything else to `_`.
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_makes_valid_function_names() {
        assert_eq!(sanitize_tool_name("fs_read"), "fs_read");
        assert_eq!(
            sanitize_tool_name("github.create issue"),
            "github_create_issue"
        );
        assert_eq!(sanitize_tool_name("a/b:c"), "a_b_c");
    }

    #[test]
    fn extract_content_joins_text_parts() {
        let res = json!({
            "content": [
                { "type": "text", "text": "line one" },
                { "type": "text", "text": "line two" },
                { "type": "image", "data": "…" },
            ]
        });
        assert_eq!(
            extract_content_text(&res),
            "line one\nline two\n[image content omitted]"
        );
        assert_eq!(extract_content_text(&json!({})), "");
    }

    #[test]
    fn rpc_error_formats_code() {
        let e = json!({ "code": -32601, "message": "Method not found" });
        assert_eq!(rpc_error_message(&e), "Method not found (code -32601)");
        assert_eq!(rpc_error_message(&json!({})), "unknown error");
    }

    #[test]
    fn sse_parse_finds_the_matching_id() {
        // A response for id 2, preceded by an unrelated notification.
        let body = "\
event: message
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}

event: message
data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}

";
        let v = parse_sse_for_id(body, 2).unwrap();
        assert!(v.get("result").is_some());
        // A multi-line data payload is reassembled.
        let multi = "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":7,\"result\":1}\n\n";
        assert_eq!(parse_sse_for_id(multi, 7).unwrap()["result"], 1);
        // No matching id → error.
        assert!(parse_sse_for_id(body, 99).is_err());
    }

    /// A newline-delimited JSON-RPC MCP server that advertises one read-only
    /// `echo` tool. `-u` keeps stdout unbuffered so responses aren't withheld.
    #[cfg(unix)]
    const MOCK_SERVER: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    m = json.loads(line); i = m.get("id"); method = m.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":i,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"mock","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":i,"result":{"tools":[{"name":"echo","description":"Echo the input back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":True}}]}})
    elif method == "tools/call":
        args = m.get("params",{}).get("arguments",{})
        send({"jsonrpc":"2.0","id":i,"result":{"content":[{"type":"text","text":"echo: "+str(args.get("text",""))}],"isError":False}})
"#;

    #[cfg(unix)]
    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|exe| {
            std::process::Command::new(exe)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
    }

    // The MCP client is cross-platform; this end-to-end test is unix-only to
    // avoid Windows python/newline-translation flakiness in CI (the pure-logic
    // tests above run everywhere).
    #[cfg(unix)]
    #[tokio::test]
    async fn connects_lists_and_calls_a_mock_stdio_server() {
        let Some(py) = python() else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let args = vec!["-u".to_string(), "-c".to_string(), MOCK_SERVER.to_string()];
        let (_client, tools) = McpClient::connect_stdio("mock", py, &args, &[])
            .await
            .expect("connect + handshake + tools/list");

        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name(), "mock_echo");
        assert!(tool.read_only());
        assert!(tool.description().contains("Echo"));

        let ctx = ToolContext::new(".");
        let out = tool
            .execute(json!({ "text": "hi there" }), &ctx)
            .await
            .expect("tools/call");
        assert_eq!(out, "echo: hi there");
    }
}

//! Minimal MCP (Model Context Protocol) client.
//!
//! Connects to an MCP server, runs the `initialize` handshake, discovers its
//! tools (`tools/list`), and exposes each as a [`Tool`] the model can call
//! (`tools/call`). If the server advertises `resources` / `prompts`
//! capabilities, list/read/get operations are exposed as tools too. Three
//! transports:
//!
//! - **stdio** — spawn a process; newline-delimited JSON-RPC 2.0 on its
//!   stdin/stdout. One background reader routes responses to pending requests by
//!   id, so calls can be concurrent.
//! - **Streamable HTTP** — POST each JSON-RPC request to a single URL; the
//!   response is either `application/json` (one message) or an SSE stream
//!   (`text/event-stream`). The server's `Mcp-Session-Id` header is echoed back
//!   on later requests.
//! - **legacy HTTP+SSE** — open a persistent GET stream that first emits an
//!   `endpoint` event (the POST URL) then carries server→client messages;
//!   requests are POSTed to that URL and their responses arrive back on the
//!   stream, routed by id like stdio.
//!
//! Scope: no server-initiated requests over Streamable HTTP (we don't open the
//! optional GET stream there).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot, watch};

use hrdr_llm::SseDecoder;

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
    Sse(SseTransport),
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

/// Legacy HTTP+SSE transport: a persistent GET stream carries server→client
/// messages (and the initial `endpoint` event giving the POST URL); requests are
/// POSTed to that URL and their responses arrive back on the stream, routed by id
/// like stdio.
struct SseTransport {
    http: reqwest::Client,
    headers: HeaderMap,
    /// POST endpoint from the `endpoint` SSE event (`None` until received).
    post_url: watch::Receiver<Option<String>>,
    pending: Pending,
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
        let map = build_headers(headers)?;
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

    /// Connect over the legacy HTTP+SSE transport: open the SSE GET stream at
    /// `sse_url`, wait for its `endpoint` event, then POST requests there.
    pub async fn connect_sse(
        server: &str,
        sse_url: &str,
        headers: &[(String, String)],
    ) -> Result<(Arc<Self>, Vec<Arc<dyn Tool>>)> {
        let http = reqwest::Client::new();
        let map = build_headers(headers)?;
        let base =
            reqwest::Url::parse(sse_url).with_context(|| format!("bad MCP url '{sse_url}'"))?;
        let resp = http
            .get(sse_url)
            .headers(map.clone())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .context("opening SSE stream")?
            .error_for_status()
            .context("opening SSE stream")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ep_tx, ep_rx) = watch::channel::<Option<String>>(None);
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut stream = resp.bytes_stream();
                // Use the shared SseDecoder for byte-safe incremental parsing:
                // raw bytes are fed directly (no lossy UTF-8 conversion), and
                // the decoder handles chunk boundaries including mid-codepoint
                // splits via per-line buffering.
                let mut decoder = SseDecoder::new();
                while let Some(Ok(chunk)) = stream.next().await {
                    decoder.push(&chunk);
                    for ev in decoder.drain() {
                        if ev.event.as_deref() == Some("endpoint") {
                            if let Ok(u) = base.join(ev.data.trim()) {
                                let _ = ep_tx.send(Some(u.to_string()));
                            }
                        } else if let Ok(msg) = serde_json::from_str::<Value>(&ev.data)
                            && let Some(id) = msg.get("id").and_then(Value::as_u64)
                            && let Some(tx) = pending.lock().await.remove(&id)
                        {
                            let _ = tx.send(msg);
                        }
                    }
                }
                pending.lock().await.clear();
            });
        }

        // Wait for the server to announce its POST endpoint.
        let mut ready = ep_rx.clone();
        tokio::time::timeout(HANDSHAKE_TIMEOUT, ready.wait_for(|v| v.is_some()))
            .await
            .map_err(|_| anyhow!("MCP '{server}': no `endpoint` event before timeout"))?
            .map_err(|_| anyhow!("MCP '{server}': SSE stream closed during handshake"))?;

        let client = Arc::new(Self {
            server: server.to_string(),
            next_id: AtomicU64::new(1),
            transport: Transport::Sse(SseTransport {
                http,
                headers: map,
                post_url: ep_rx,
                pending,
            }),
        });
        let tools = client.clone().handshake_and_list().await?;
        Ok((client, tools))
    }

    /// Run `initialize` + `tools/list`, wrapping each discovered tool. If the
    /// server advertises `resources` / `prompts` capabilities, add
    /// list/read/get tools for those too.
    async fn handshake_and_list(self: Arc<Self>) -> Result<Vec<Arc<dyn Tool>>> {
        let caps = self.initialize().await?;
        let server = self.server.clone();
        let op_tool = |op_name: &str, desc: &str, schema: Value, op: McpOp| -> Arc<dyn Tool> {
            Arc::new(McpTool {
                client: self.clone(),
                exposed_name: Box::leak(
                    sanitize_tool_name(&format!("{server}_{op_name}")).into_boxed_str(),
                ),
                description: Box::leak(desc.to_string().into_boxed_str()),
                schema,
                op,
                read_only: true,
            })
        };

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for d in self.list_tools().await? {
            tools.push(Arc::new(McpTool {
                client: self.clone(),
                // Leaked once at startup (bounded): the registry keys on
                // `&'static str`, and these tools live for the process.
                exposed_name: Box::leak(
                    sanitize_tool_name(&format!("{server}_{}", d.name)).into_boxed_str(),
                ),
                description: Box::leak(d.description.into_boxed_str()),
                schema: d.schema,
                op: McpOp::Tool(d.name),
                read_only: d.read_only,
            }));
        }
        if caps.get("resources").is_some() {
            tools.push(op_tool(
                "list_resources",
                "List the resources this MCP server exposes (each a `uri` + name).",
                json!({ "type": "object" }),
                McpOp::ListResources,
            ));
            tools.push(op_tool(
                "read_resource",
                "Read one MCP resource by `uri` (from list_resources).",
                json!({
                    "type": "object",
                    "properties": { "uri": { "type": "string", "description": "Resource URI." } },
                    "required": ["uri"]
                }),
                McpOp::ReadResource,
            ));
        }
        if caps.get("prompts").is_some() {
            tools.push(op_tool(
                "list_prompts",
                "List the prompt templates this MCP server exposes (name + arguments).",
                json!({ "type": "object" }),
                McpOp::ListPrompts,
            ));
            tools.push(op_tool(
                "get_prompt",
                "Render an MCP prompt template by `name` with optional `arguments`.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Prompt name." },
                        "arguments": { "type": "object", "description": "Template arguments." }
                    },
                    "required": ["name"]
                }),
                McpOp::GetPrompt,
            ));
        }
        Ok(tools)
    }

    /// Send a JSON-RPC request over whichever transport, and extract its result.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let msg = match &self.transport {
            Transport::Stdio(t) => stdio_request(t, id, req, timeout).await,
            Transport::Http(t) => http_request(t, id, req, timeout).await,
            Transport::Sse(t) => sse_request(t, id, req, timeout).await,
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
            Transport::Sse(t) => {
                // Clone out of the watch guard before the await (guard isn't Send).
                let url = t.post_url.borrow().clone();
                if let Some(url) = url {
                    let _ = t
                        .http
                        .post(&url)
                        .headers(t.headers.clone())
                        .json(&msg)
                        .send()
                        .await;
                }
            }
        }
    }

    /// Run the handshake; returns the server's advertised `capabilities`.
    async fn initialize(&self) -> Result<Value> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "hrdr", "version": env!("CARGO_PKG_VERSION") },
        });
        let res = self
            .request("initialize", params, HANDSHAKE_TIMEOUT)
            .await?;
        self.notify("notifications/initialized", json!({})).await;
        Ok(res
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({})))
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

    async fn list_resources(&self) -> Result<String> {
        let res = self
            .request("resources/list", json!({}), HANDSHAKE_TIMEOUT)
            .await?;
        let mut out = String::new();
        for r in res
            .get("resources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let uri = r.get("uri").and_then(Value::as_str).unwrap_or("");
            let name = r.get("name").and_then(Value::as_str).unwrap_or("");
            let desc = r.get("description").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("{uri}\t{name}"));
            if !desc.is_empty() {
                out.push_str(&format!("\t{desc}"));
            }
            out.push('\n');
        }
        Ok(if out.is_empty() {
            "(no resources)".to_string()
        } else {
            out
        })
    }

    async fn read_resource(&self, uri: &str) -> Result<String> {
        let res = self
            .request("resources/read", json!({ "uri": uri }), CALL_TIMEOUT)
            .await?;
        let mut out = String::new();
        for c in res
            .get("contents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(t) = c.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            } else if c.get("blob").is_some() {
                out.push_str("[binary resource content omitted]");
            }
        }
        Ok(out)
    }

    async fn list_prompts(&self) -> Result<String> {
        let res = self
            .request("prompts/list", json!({}), HANDSHAKE_TIMEOUT)
            .await?;
        let mut out = String::new();
        for p in res
            .get("prompts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let desc = p.get("description").and_then(Value::as_str).unwrap_or("");
            let args: Vec<&str> = p
                .get("arguments")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("name").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            out.push_str(&format!("{name}({})", args.join(", ")));
            if !desc.is_empty() {
                out.push_str(&format!("\t{desc}"));
            }
            out.push('\n');
        }
        Ok(if out.is_empty() {
            "(no prompts)".to_string()
        } else {
            out
        })
    }

    async fn get_prompt(&self, name: &str, arguments: Value) -> Result<String> {
        let res = self
            .request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
                CALL_TIMEOUT,
            )
            .await?;
        let mut out = String::new();
        for m in res
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            let text = m
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("{role}: {text}"));
        }
        Ok(out)
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
    // On a send failure the id would otherwise leak in `pending` until the
    // reader task exits (the reader only removes ids it sees a response for),
    // so drop it here before returning — mirroring the timeout arm below.
    if t.stdin_tx.send(req.to_string()).is_err() {
        t.pending.lock().await.remove(&id);
        bail!("server is not running");
    }
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

/// Legacy HTTP+SSE: POST the request to the endpoint; the response arrives back
/// on the persistent SSE stream and is delivered via `pending`.
async fn sse_request(t: &SseTransport, id: u64, req: Value, timeout: Duration) -> Result<Value> {
    let post_url = t
        .post_url
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("no endpoint"))?;
    let (tx, rx) = oneshot::channel();
    t.pending.lock().await.insert(id, tx);
    // A transport error on `send` (or a non-success status below) early-returns,
    // so drop the just-inserted id before propagating — otherwise it leaks in
    // `pending` until the reader task tears the map down.
    let resp = match t
        .http
        .post(&post_url)
        .headers(t.headers.clone())
        .json(&req)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            t.pending.lock().await.remove(&id);
            return Err(anyhow::Error::new(e).context("request failed"));
        }
    };
    if !resp.status().is_success() {
        t.pending.lock().await.remove(&id);
        bail!("HTTP {}", resp.status());
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(msg)) => Ok(msg),
        Ok(Err(_)) => bail!("connection closed"),
        Err(_) => {
            t.pending.lock().await.remove(&id);
            bail!("timed out")
        }
    }
}

/// Build a [`HeaderMap`] from `(name, value)` pairs (config auth headers).
fn build_headers(headers: &[(String, String)]) -> Result<HeaderMap> {
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

/// Find the JSON-RPC message with `id` in an SSE stream body.
///
/// Uses [`SseDecoder`] for correct blank-line-terminated event grouping and
/// multi-line `data:` folding (mirrors the Streamable-HTTP inline SSE path).
/// A trailing `\n\n` is pushed after the body to flush any event that was not
/// terminated in the buffer (some servers omit the final blank line).
fn parse_sse_for_id(body: &str, id: u64) -> Result<Value> {
    let mut dec = SseDecoder::new();
    dec.push(body.as_bytes());
    // Force-flush a trailing event that isn't blank-line-terminated.
    dec.push(b"\n\n");
    for ev in dec.drain() {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.data)
            && v.get("id").and_then(Value::as_u64) == Some(id)
        {
            return Ok(v);
        }
    }
    bail!("no response for request {id} in the SSE stream")
}

/// What an [`McpTool`] does when the model calls it.
enum McpOp {
    /// `tools/call` with this server-side tool name.
    Tool(String),
    ListResources,
    ReadResource,
    ListPrompts,
    GetPrompt,
}

/// One MCP capability, exposed to the model as a native [`Tool`] — either a
/// server tool or a resource/prompt list/read/get operation.
struct McpTool {
    client: Arc<McpClient>,
    exposed_name: &'static str,
    description: &'static str,
    schema: Value,
    op: McpOp,
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
        let out = match &self.op {
            McpOp::Tool(name) => self.client.call_tool(name, args).await?,
            McpOp::ListResources => self.client.list_resources().await?,
            McpOp::ReadResource => {
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("read_resource needs a `uri` argument"))?;
                self.client.read_resource(uri).await?
            }
            McpOp::ListPrompts => self.client.list_prompts().await?,
            McpOp::GetPrompt => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("get_prompt needs a `name` argument"))?;
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                self.client.get_prompt(name, arguments).await?
            }
        };
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

    // The stdio/sse request helpers insert a oneshot into `pending` *before*
    // sending, and must remove it on every early-return (send error, non-success
    // status, timeout) or the id leaks until the reader task tears the map down.
    // Constructing a half-open transport to force a real send failure needs a
    // live child, so this asserts the insert→remove bookkeeping invariant on the
    // shared `Pending` type the helpers use.
    #[tokio::test]
    async fn pending_id_removed_on_send_failure_path() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let id = 7u64;
        let (tx, _rx) = oneshot::channel::<Value>();
        pending.lock().await.insert(id, tx);
        assert!(pending.lock().await.contains_key(&id));
        // Simulated send failure: the fix removes the id before returning.
        pending.lock().await.remove(&id);
        assert!(
            pending.lock().await.is_empty(),
            "send-error path must not leak the pending id"
        );
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

    /// A mock MCP server that advertises `tools` + `resources` + `prompts`
    /// capabilities and speaks all three transports. `argv[1]` selects the mode:
    /// `stdio` (newline-delimited JSON-RPC on stdin/stdout), `http`
    /// (Streamable-HTTP, one JSON response per POST), or `sse` (legacy HTTP+SSE:
    /// a GET stream that emits the `endpoint` event then carries responses to
    /// requests POSTed there). HTTP modes print their ephemeral port on the first
    /// stdout line. `-u` keeps stdout unbuffered so responses aren't withheld.
    #[cfg(unix)]
    const MOCK_SERVER: &str = r#"
import sys, json, os

# MOCK_NOCAPS drops the resources/prompts capabilities (so the client shouldn't
# expose those op-tools). MOCK_EMPTY makes the list methods return nothing.
CAPS = {"tools":{}} if os.environ.get("MOCK_NOCAPS") else {"tools":{},"resources":{},"prompts":{}}
EMPTY = bool(os.environ.get("MOCK_EMPTY"))

def handle(m, session=None):
    i = m.get("id"); method = m.get("method")
    if method == "initialize":
        return {"jsonrpc":"2.0","id":i,"result":{"protocolVersion":"2025-06-18","capabilities":CAPS,"serverInfo":{"name":"mock","version":"0"}}}
    if method == "tools/list":
        return {"jsonrpc":"2.0","id":i,"result":{"tools":[
            {"name":"echo","description":"Echo the input back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":True}},
            {"name":"boom","description":"Always fails","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":False}},
            {"name":"rpcfail","description":"Returns a JSON-RPC error","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":True}},
            {"name":"pic","description":"Returns an image part","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":True}},
            {"name":"whoami","description":"Echoes the session id the server saw","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":True}}]}}
    if method == "tools/call":
        p = m.get("params",{}); name = p.get("name",""); args = p.get("arguments",{})
        if name == "boom":
            return {"jsonrpc":"2.0","id":i,"result":{"content":[{"type":"text","text":"kaboom"}],"isError":True}}
        if name == "rpcfail":
            return {"jsonrpc":"2.0","id":i,"error":{"code":-32000,"message":"tool exploded"}}
        if name == "pic":
            return {"jsonrpc":"2.0","id":i,"result":{"content":[{"type":"image","data":"aGk=","mimeType":"image/png"}],"isError":False}}
        if name == "whoami":
            return {"jsonrpc":"2.0","id":i,"result":{"content":[{"type":"text","text":str(session or "(none)")}],"isError":False}}
        return {"jsonrpc":"2.0","id":i,"result":{"content":[{"type":"text","text":"echo: "+str(args.get("text",""))}],"isError":False}}
    if method == "resources/list":
        res = [] if EMPTY else [{"uri":"file:///readme","name":"readme","description":"the readme"},{"uri":"blob://logo","name":"logo"}]
        return {"jsonrpc":"2.0","id":i,"result":{"resources":res}}
    if method == "resources/read":
        uri = m.get("params",{}).get("uri","")
        if uri.startswith("blob://"):
            return {"jsonrpc":"2.0","id":i,"result":{"contents":[{"uri":uri,"blob":"aGk="}]}}
        return {"jsonrpc":"2.0","id":i,"result":{"contents":[{"uri":uri,"text":"resource body for "+uri}]}}
    if method == "prompts/list":
        pr = [] if EMPTY else [{"name":"greet","description":"greet someone","arguments":[{"name":"who","required":True}]}]
        return {"jsonrpc":"2.0","id":i,"result":{"prompts":pr}}
    if method == "prompts/get":
        args = m.get("params",{}).get("arguments",{})
        return {"jsonrpc":"2.0","id":i,"result":{"messages":[{"role":"user","content":{"type":"text","text":"Hello "+str(args.get("who",""))}}]}}
    if i is None:
        return None  # a notification (e.g. notifications/initialized)
    return {"jsonrpc":"2.0","id":i,"error":{"code":-32601,"message":"Method not found"}}

def run_stdio():
    for line in sys.stdin:
        line = line.strip()
        if not line: continue
        r = handle(json.loads(line))
        if r is not None:
            sys.stdout.write(json.dumps(r) + "\n"); sys.stdout.flush()

def run_http():
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a): pass
        def do_POST(self):
            n = int(self.headers.get("Content-Length","0"))
            m = json.loads(self.rfile.read(n) or "{}")
            r = handle(m, self.headers.get("Mcp-Session-Id"))
            if r is None:
                self.send_response(202); self.end_headers(); return
            body = json.dumps(r).encode()
            self.send_response(200)
            self.send_header("Content-Type","application/json")
            if m.get("method") == "initialize":
                self.send_header("Mcp-Session-Id","sess-1")
            self.send_header("Content-Length",str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    srv = ThreadingHTTPServer(("127.0.0.1",0), H)
    print(srv.server_address[1], flush=True)
    srv.serve_forever()

def run_sse():
    import queue
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
    q = queue.Queue()
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a): pass
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type","text/event-stream")
            self.send_header("Cache-Control","no-cache")
            self.end_headers()
            self.wfile.write(b"event: endpoint\ndata: /messages\n\n"); self.wfile.flush()
            while True:
                try:
                    msg = q.get(timeout=1)
                except queue.Empty:
                    try:
                        self.wfile.write(b": ping\n\n"); self.wfile.flush()
                    except Exception:
                        return
                    continue
                try:
                    self.wfile.write(("event: message\ndata: "+json.dumps(msg)+"\n\n").encode()); self.wfile.flush()
                except Exception:
                    return
        def do_POST(self):
            n = int(self.headers.get("Content-Length","0"))
            r = handle(json.loads(self.rfile.read(n) or "{}"))
            self.send_response(202); self.end_headers()
            if r is not None:
                q.put(r)
    srv = ThreadingHTTPServer(("127.0.0.1",0), H)
    print(srv.server_address[1], flush=True)
    srv.serve_forever()

mode = sys.argv[1] if len(sys.argv) > 1 else "stdio"
{"http": run_http, "sse": run_sse}.get(mode, run_stdio)()
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

    /// Kills its child process (and reaps it) on drop, so an HTTP/SSE mock server
    /// doesn't outlive the test even if an assertion panics.
    #[cfg(unix)]
    struct Killer(std::process::Child);
    #[cfg(unix)]
    impl Drop for Killer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawn the mock server in `http`/`sse` mode; returns the child (as a
    /// [`Killer`] guard) and the port it bound. `None` if python is unavailable.
    #[cfg(unix)]
    fn spawn_mock_server(mode: &str) -> Option<(Killer, u16)> {
        use std::io::BufRead;
        let py = python()?;
        let mut child = std::process::Command::new(py)
            .args(["-u", "-c", MOCK_SERVER, mode])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        // (env toggles like MOCK_NOCAPS are only exercised over stdio.)
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let port: u16 = line.trim().parse().ok()?;
        Some((Killer(child), port))
    }

    /// Exercise every capability the mock advertises: the `echo` tool, resource
    /// list/read, and prompt list/get. Shared across all three transports.
    #[cfg(unix)]
    async fn exercise_all(server: &str, tools: Vec<Arc<dyn Tool>>) {
        let by = |suffix: &str| {
            let want = format!("{server}_{suffix}");
            tools
                .iter()
                .find(|t| t.name() == want)
                .cloned()
                .unwrap_or_else(|| panic!("missing tool {want}"))
        };
        let ctx = ToolContext::new(".");

        let echo = by("echo");
        assert!(echo.read_only());
        assert!(echo.description().contains("Echo"));
        assert_eq!(
            echo.execute(json!({ "text": "hi there" }), &ctx)
                .await
                .unwrap(),
            "echo: hi there"
        );

        // A non-read-only tool that reports `isError` → `execute` surfaces it.
        let boom = by("boom");
        assert!(!boom.read_only());
        let err = boom.execute(json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("kaboom"), "err: {err}");

        // A JSON-RPC `error` object (distinct from `isError` content) surfaces too.
        let rpc_err = by("rpcfail").execute(json!({}), &ctx).await.unwrap_err();
        assert!(
            rpc_err.to_string().contains("tool exploded"),
            "err: {rpc_err}"
        );

        // Non-text (image) tool content is noted, not dumped inline.
        let pic = by("pic").execute(json!({}), &ctx).await.unwrap();
        assert_eq!(pic, "[image content omitted]");

        // Concurrent calls are routed back to the right caller by id.
        let outs = futures_util::future::join_all((0..8).map(|n| {
            let echo = echo.clone();
            let ctx = &ctx;
            async move {
                echo.execute(json!({ "text": format!("n{n}") }), ctx)
                    .await
                    .unwrap()
            }
        }))
        .await;
        for (n, out) in outs.iter().enumerate() {
            assert_eq!(*out, format!("echo: n{n}"));
        }

        let listed = by("list_resources").execute(json!({}), &ctx).await.unwrap();
        assert!(listed.contains("file:///readme"), "resources: {listed}");
        let read = by("read_resource")
            .execute(json!({ "uri": "file:///readme" }), &ctx)
            .await
            .unwrap();
        assert_eq!(read, "resource body for file:///readme");
        // A binary (blob) resource is noted, not dumped.
        let blob = by("read_resource")
            .execute(json!({ "uri": "blob://logo" }), &ctx)
            .await
            .unwrap();
        assert_eq!(blob, "[binary resource content omitted]");

        let prompts = by("list_prompts").execute(json!({}), &ctx).await.unwrap();
        assert!(prompts.contains("greet(who)"), "prompts: {prompts}");
        let rendered = by("get_prompt")
            .execute(
                json!({ "name": "greet", "arguments": { "who": "Sam" } }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(rendered, "user: Hello Sam");
    }

    // The MCP client is cross-platform; these end-to-end tests are unix-only to
    // avoid Windows python/newline-translation flakiness in CI (the pure-logic
    // tests above run everywhere).
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_transport_tools_resources_prompts() {
        let Some(py) = python() else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let args = vec![
            "-u".to_string(),
            "-c".to_string(),
            MOCK_SERVER.to_string(),
            "stdio".to_string(),
        ];
        let (_client, tools) = McpClient::connect_stdio("stdio", py, &args, &[])
            .await
            .expect("connect + handshake + tools/list");
        // 5 real tools + {list,read}_resources + {list,get}_prompt.
        assert_eq!(tools.len(), 9);
        exercise_all("stdio", tools).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streamable_http_transport_tools_resources_prompts() {
        let Some((_guard, port)) = spawn_mock_server("http") else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let url = format!("http://127.0.0.1:{port}/mcp");
        let (_client, tools) = McpClient::connect_http("http", &url, &[])
            .await
            .expect("connect over Streamable HTTP");
        assert_eq!(tools.len(), 9);
        // The `Mcp-Session-Id` the server returned on `initialize` is resent on
        // later requests: `whoami` echoes back the session id it saw.
        let ctx = ToolContext::new(".");
        let whoami = tools.iter().find(|t| t.name() == "http_whoami").unwrap();
        assert_eq!(whoami.execute(json!({}), &ctx).await.unwrap(), "sess-1");
        exercise_all("http", tools).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_http_sse_transport_tools_resources_prompts() {
        let Some((_guard, port)) = spawn_mock_server("sse") else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let url = format!("http://127.0.0.1:{port}/sse");
        let (_client, tools) = McpClient::connect_sse("sse", &url, &[])
            .await
            .expect("connect over legacy HTTP+SSE");
        assert_eq!(tools.len(), 9);
        exercise_all("sse", tools).await;
    }

    /// Spawn the stdio mock with extra env, returning its discovered tools.
    #[cfg(unix)]
    async fn connect_stdio_mock(
        server: &str,
        env: &[(String, String)],
    ) -> Option<Vec<Arc<dyn Tool>>> {
        let py = python()?;
        let args = vec![
            "-u".to_string(),
            "-c".to_string(),
            MOCK_SERVER.to_string(),
            "stdio".to_string(),
        ];
        // Each `McpTool` holds an `Arc<McpClient>`, so the returned `tools` keep
        // the connection (and its child) alive without threading the client out.
        let (_client, tools) = McpClient::connect_stdio(server, py, &args, env)
            .await
            .expect("connect + handshake + tools/list");
        Some(tools)
    }

    // A server that doesn't advertise `resources`/`prompts` gets no op-tools.
    #[cfg(unix)]
    #[tokio::test]
    async fn absent_capabilities_omit_resource_and_prompt_tools() {
        let env = vec![("MOCK_NOCAPS".to_string(), "1".to_string())];
        let Some(tools) = connect_stdio_mock("nocaps", &env).await else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        // Only the real tools — no list/read/get op-tools.
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(tools.len(), 5, "tools: {names:?}");
        assert!(names.contains(&"nocaps_echo"));
        assert!(names.contains(&"nocaps_boom"));
        assert!(
            !names
                .iter()
                .any(|n| n.contains("resource") || n.contains("prompt")),
            "unexpected op-tool: {names:?}"
        );
    }

    // Empty resource/prompt lists render as a friendly placeholder.
    #[cfg(unix)]
    #[tokio::test]
    async fn empty_resource_and_prompt_lists_render_placeholders() {
        let env = vec![("MOCK_EMPTY".to_string(), "1".to_string())];
        let Some(tools) = connect_stdio_mock("empty", &env).await else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let ctx = ToolContext::new(".");
        let by = |suffix: &str| {
            let want = format!("empty_{suffix}");
            tools.iter().find(|t| t.name() == want).cloned().unwrap()
        };
        assert_eq!(
            by("list_resources").execute(json!({}), &ctx).await.unwrap(),
            "(no resources)"
        );
        assert_eq!(
            by("list_prompts").execute(json!({}), &ctx).await.unwrap(),
            "(no prompts)"
        );
    }

    /// Test 6a — focused `tools/call` round-trip over stdio.
    ///
    /// The comprehensive `stdio_transport_tools_resources_prompts` test exercises
    /// `tools/call` as part of a broader capabilities check.  This test is
    /// intentionally narrow: it connects, discovers tools, calls *just* the
    /// `echo` tool, and verifies the returned content exactly.
    ///
    /// Regression caught: any breakage in how `tools/call` requests are
    /// formatted or how their `content[].text` values are extracted — without
    /// requiring resources/prompts to also be working.
    #[cfg(unix)]
    #[tokio::test]
    async fn tools_call_round_trip_over_stdio() {
        let Some(py) = python() else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let args = vec![
            "-u".to_string(),
            "-c".to_string(),
            MOCK_SERVER.to_string(),
            "stdio".to_string(),
        ];
        let (_client, tools) = McpClient::connect_stdio("rt", py, &args, &[])
            .await
            .expect("connect + handshake + tools/list");
        let ctx = ToolContext::new(".");
        let echo = tools
            .iter()
            .find(|t| t.name() == "rt_echo")
            .expect("echo tool must be discovered via tools/list");
        let result = echo
            .execute(json!({ "text": "round-trip" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            result, "echo: round-trip",
            "tools/call must relay the server's content verbatim"
        );
    }

    /// Test 6b — `stdio_request` removes the pending id on send failure.
    ///
    /// The real `stdio_request` code path (not just the `Pending` bookkeeping
    /// invariant checked in `pending_id_removed_on_send_failure_path`) must be
    /// exercised: insert the id → attempt to send → channel broken → remove the
    /// id → return `Err`.
    ///
    /// Regression caught: if the early-return in `stdio_request` were removed
    /// (e.g. the `if ... .is_err()` branch dropped), the id would remain in the
    /// pending map forever.  On the *next* call the reader task would have
    /// exited (reader clears pending on EOF), but future calls with the same id
    /// would insert a new sender that the dead reader could never fulfill —
    /// those callers would block until timeout.  The explicit remove-before-
    /// return prevents this leak regardless of reader-task timing.
    ///
    /// Implementation note: constructing `StdioTransport` directly is possible
    /// because `mod tests` is a child module of `mcp.rs` and can therefore
    /// access its private fields.  The channel receiver is dropped before the
    /// transport is used, guaranteeing `stdin_tx.send()` returns `Err`
    /// synchronously — no timing dependency.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_request_send_error_removes_pending_id() {
        use tokio::sync::mpsc;

        // Drop the receiver immediately; every subsequent send will fail.
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();
        drop(stdin_rx);

        // A trivial child satisfies the `_child: Child` field.  Its stdio is
        // irrelevant since the send fails before anything is written.
        let child = Command::new("sh")
            .args(["-c", ""])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("sh must be available on unix");

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let t = StdioTransport {
            stdin_tx,
            pending: pending.clone(),
            _child: child,
        };

        let id = 77u64;
        let req = json!({"jsonrpc":"2.0","id": id,"method":"tools/call","params":{}});
        let result = stdio_request(&t, id, req, Duration::from_millis(200)).await;

        assert!(
            result.is_err(),
            "calling on a broken channel must return Err, not hang"
        );
        assert!(
            pending.lock().await.is_empty(),
            "stdio_request send-error path must remove the pending id before \
             returning (found: {:?})",
            pending.lock().await.keys().collect::<Vec<_>>()
        );
    }
}

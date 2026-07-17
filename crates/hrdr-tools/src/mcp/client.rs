use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::header::ACCEPT;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, watch};

use hrdr_llm::SseDecoder;

use crate::Tool;

use super::McpClient;
use super::{
    CALL_TIMEOUT, Discovered, HANDSHAKE_TIMEOUT, HttpTransport, McpOp, McpTool, PROTOCOL_VERSION,
    Pending, SseTransport, StdioTransport, Transport, build_headers, http_request, http_send,
    sse_request, stdio_request,
};
use super::{extract_content_text, response_id, rpc_error_message, sanitize_tool_name};

/// Read one newline-delimited stdio message without ever buffering more than
/// the protocol cap. Oversized lines are drained through their newline so the
/// next valid message remains parseable. An empty vector means EOF; `None`
/// means an oversized line was discarded.
async fn read_stdio_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if out.is_empty() && !oversized {
                Ok(Some(Vec::new()))
            } else if oversized {
                Ok(None)
            } else {
                Ok(Some(out))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if !oversized {
            let remaining = super::MAX_MCP_MESSAGE_BYTES.saturating_sub(out.len());
            if take > remaining {
                oversized = true;
                out.clear();
            } else {
                out.extend_from_slice(&available[..take]);
            }
        }
        let ended = available[take - 1] == b'\n';
        reader.consume(take);
        if ended {
            return if oversized { Ok(None) } else { Ok(Some(out)) };
        }
    }
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

        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<String>(super::MCP_STDIN_CAP);
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

        let pending: Pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        {
            let pending = pending.clone();
            // A clone so the reader task can answer server-initiated `ping`
            // requests without needing anything from `StdioTransport` (which
            // isn't built yet, and shouldn't be raced against from here).
            let stdin_tx = stdin_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_stdio_line_capped(&mut reader).await {
                        Ok(None) => continue,
                        Err(_) => break,
                        Ok(Some(buf)) if buf.is_empty() => break,
                        Ok(Some(buf)) => {
                            let Ok(line) = std::str::from_utf8(&buf) else {
                                continue; // not valid UTF-8: not JSON either
                            };
                            let Ok(msg) = serde_json::from_str::<Value>(line.trim_end()) else {
                                continue; // skip non-JSON (some servers emit banners)
                            };
                            // A message with a `method` field is a
                            // server-initiated request/notification, never a
                            // response — id spaces are per-sender, so its id
                            // can legitimately collide with one of ours. Only
                            // route messages that aren't requests.
                            if let Some(id) = response_id(&msg) {
                                if let Some(tx) = pending.lock().await.remove(&id) {
                                    let _ = tx.send(msg);
                                }
                                // else: response to an id we're no longer
                                // waiting on (already timed out) — drop it.
                            } else if msg.get("method").and_then(Value::as_str) == Some("ping")
                                && let Some(id) = msg.get("id").cloned()
                            {
                                // Cheap best-effort answer so a conformant
                                // server doesn't consider us unresponsive.
                                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                                // Best-effort, non-blocking: the reader task must
                                // never block on stdin capacity (that would stall
                                // response routing). Dropping a ping reply under
                                // backpressure is harmless.
                                let _ = stdin_tx.try_send(reply.to_string());
                            }
                            // other notifications / server-initiated requests
                            // are otherwise ignored (v1).
                        }
                    }
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
    /// Build the `reqwest::Client` for an HTTP/SSE MCP transport.
    ///
    /// Fallible builder, not `reqwest::Client::new()` — the latter panics if
    /// the TLS backend fails to initialize, turning an environment
    /// misconfiguration into a process abort instead of a recoverable error. No
    /// client-wide `.timeout()`: `request()` already races each send against
    /// the caller-chosen deadline (HANDSHAKE_TIMEOUT vs. the much longer
    /// CALL_TIMEOUT for tool calls), and a client-wide timeout would clobber
    /// that distinction.
    fn build_http(server: &str) -> Result<reqwest::Client> {
        reqwest::Client::builder().build().map_err(|e| {
            anyhow!(
                "MCP '{server}': building HTTP client (TLS backend missing or misconfigured): {e}"
            )
        })
    }

    pub async fn connect_http(
        server: &str,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<(Arc<Self>, Vec<Arc<dyn Tool>>)> {
        let map = build_headers(headers)?;
        let http = Self::build_http(server)?;
        let client = Arc::new(Self {
            server: server.to_string(),
            next_id: AtomicU64::new(1),
            transport: Transport::Http(HttpTransport {
                http,
                url: url.to_string(),
                headers: map,
                session: std::sync::Mutex::new(None),
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
        let http = Self::build_http(server)?;
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

        let pending: Pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
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
                // Per-message cap: bytes buffered since the last complete event.
                // Reset to 0 on every successful drain, so it bounds a single
                // oversized message — NOT the stream's whole lifetime. (A
                // cumulative cap here would silently retire the long-lived SSE
                // channel mid-session once total traffic crossed the limit,
                // dropping every waiter and wedging the server.)
                let mut undecoded_bytes = 0usize;
                while let Some(Ok(chunk)) = stream.next().await {
                    undecoded_bytes = undecoded_bytes.saturating_add(chunk.len());
                    if undecoded_bytes > super::MAX_MCP_MESSAGE_BYTES {
                        break;
                    }
                    if decoder.push(&chunk).is_err() {
                        break;
                    }
                    let events = decoder.drain();
                    if !events.is_empty() {
                        undecoded_bytes = 0;
                    }
                    for ev in events {
                        if ev.event.as_deref() == Some("endpoint") {
                            if let Ok(u) = base.join(ev.data.trim()) {
                                let _ = ep_tx.send(Some(u.to_string()));
                            }
                        } else if let Ok(msg) = serde_json::from_str::<Value>(&ev.data)
                            && let Some(id) = response_id(&msg)
                            && let Some(tx) = pending.lock().await.remove(&id)
                        {
                            // A message with a `method` field (server-initiated
                            // request/notification) is filtered out by
                            // `response_id` before we ever get here — id
                            // spaces are per-sender, so such a message's id
                            // can legitimately collide with one of ours.
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
    pub(crate) async fn handshake_and_list(self: Arc<Self>) -> Result<Vec<Arc<dyn Tool>>> {
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
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
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
    ///
    /// `initialize()` awaits this for `notifications/initialized`, so the
    /// HTTP/SSE sends are bounded by `HANDSHAKE_TIMEOUT`: without a deadline,
    /// a server that accepts the POST but never responds would wedge
    /// `connect_http`/`connect_sse` forever (stdio's send awaits only local
    /// channel capacity and errors out if the child is gone, so it needs no
    /// timeout).
    pub(crate) async fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stdio(t) => {
                // Bounded channel: await capacity (backpressure). A dead child
                // drops the receiver, so this errors out instead of hanging.
                let _ = t.stdin_tx.send(msg.to_string()).await;
            }
            Transport::Http(t) => {
                let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, http_send(t, &msg)).await;
            }
            Transport::Sse(t) => {
                // Clone out of the watch guard before the await (guard isn't Send).
                let url = t.post_url.borrow().clone();
                if let Some(url) = url {
                    let _ = tokio::time::timeout(
                        HANDSHAKE_TIMEOUT,
                        t.http
                            .post(&url)
                            .headers(t.headers.clone())
                            .json(&msg)
                            .send(),
                    )
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

    pub(crate) async fn list_tools(&self) -> Result<Vec<Discovered>> {
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

    pub(crate) async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
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

    pub(crate) async fn list_resources(&self) -> Result<String> {
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

    pub(crate) async fn read_resource(&self, uri: &str) -> Result<String> {
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

    pub(crate) async fn list_prompts(&self) -> Result<String> {
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

    pub(crate) async fn get_prompt(&self, name: &str, arguments: Value) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_line_reader_drops_oversized_line_and_recovers() {
        let input = format!(
            "{}\n{{\"ok\":true}}\n",
            "x".repeat(super::super::MAX_MCP_MESSAGE_BYTES + 1)
        );
        let mut reader = BufReader::new(input.as_bytes());
        assert!(read_stdio_line_capped(&mut reader).await.unwrap().is_none());
        let next = read_stdio_line_capped(&mut reader).await.unwrap().unwrap();
        assert_eq!(next, b"{\"ok\":true}\n");
    }

    // Regression for a MAJOR bug: `initialize()` awaits `notify()` for
    // `notifications/initialized`, and the HTTP notify path used to POST with
    // no deadline of any kind (`reqwest::Client::new()` has no timeout, and
    // `http_send` wasn't wrapped either). A server that accepts the POST but
    // never responds would wedge `connect_http` forever.
    //
    // This runs against the real clock (the crate's dev-deps don't enable
    // tokio's `test-util` feature, so `start_paused` virtual-time isn't
    // available) — it really waits out `HANDSHAKE_TIMEOUT` before asserting.
    // The outer `tokio::time::timeout` is a safety net set comfortably
    // longer: if the internal timeout regressed to "none", the assertion
    // below fails with a clear message instead of hanging the suite forever.
    #[tokio::test]
    async fn http_notify_is_bounded_by_handshake_timeout_not_unbounded() {
        // A TCP listener that accepts connections but never writes a
        // response or closes them, simulating a server that silently swallows
        // the notification POST.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                // Hold the connection open forever; never respond.
                std::mem::forget(stream);
            }
        });

        let http = reqwest::Client::builder()
            .build()
            .expect("building a plain client must not fail in tests");
        let client = McpClient {
            server: "hung".to_string(),
            next_id: AtomicU64::new(1),
            transport: Transport::Http(HttpTransport {
                http,
                url: format!("http://{addr}/mcp"),
                headers: reqwest::header::HeaderMap::new(),
                session: std::sync::Mutex::new(None),
            }),
        };

        tokio::time::timeout(
            HANDSHAKE_TIMEOUT + Duration::from_secs(30),
            client.notify("notifications/initialized", json!({})),
        )
        .await
        .expect(
            "notify() must return once its own HANDSHAKE_TIMEOUT elapses, \
             not hang until this test's much longer outer bound",
        );
    }
}

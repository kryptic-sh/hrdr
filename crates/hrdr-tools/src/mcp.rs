//! Minimal MCP (Model Context Protocol) client.
//!
//! Connects to an MCP server over the **stdio transport** — newline-delimited
//! JSON-RPC 2.0 on the server's stdin/stdout — runs the `initialize` handshake,
//! discovers its tools (`tools/list`), and exposes each as a [`Tool`] the model
//! can call (`tools/call`). One background reader routes responses to pending
//! requests by id, so calls can be concurrent.
//!
//! Scope (v1): stdio transport, tools only (no resources/prompts), no
//! server-initiated requests. HTTP/SSE transports are a follow-up.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
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

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A live connection to one MCP server (stdio transport). Dropping it kills the
/// server process (`kill_on_drop`).
pub struct McpClient {
    server: String,
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Child,
}

/// A tool advertised by an MCP server's `tools/list`.
struct Discovered {
    name: String,
    description: String,
    schema: Value,
    read_only: bool,
}

impl McpClient {
    /// Spawn `command args…` (with extra `env`), run the MCP handshake, and
    /// return the client plus its discovered tools wrapped as [`Tool`]s.
    /// `server` names the connection (used to namespace tool names + errors).
    pub async fn connect(
        server: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<(Arc<Self>, Vec<Arc<dyn Tool>>)> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // v1: server logs are discarded
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

        // Writer task: serialize outgoing messages as newline-delimited JSON.
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

        // Reader task: route responses to pending requests by id.
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue; // skip non-JSON (some servers emit banners)
                    };
                    // A response carries an id we're waiting on; notifications and
                    // server-initiated requests are ignored in v1.
                    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                        let waiter = pending.lock().await.remove(&id);
                        if let Some(tx) = waiter {
                            let res = match msg.get("error") {
                                Some(err) => Err(rpc_error_message(err)),
                                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(res);
                        }
                    }
                }
                // Stream closed: fail every outstanding request.
                for (_, tx) in pending.lock().await.drain() {
                    let _ = tx.send(Err("server closed the connection".to_string()));
                }
            });
        }

        let client = Arc::new(Self {
            server: server.to_string(),
            stdin_tx,
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
        });
        client.initialize().await?;
        let discovered = client.list_tools().await?;

        let tools: Vec<Arc<dyn Tool>> = discovered
            .into_iter()
            .map(|d| {
                let exposed = sanitize_tool_name(&format!("{server}_{}", d.name));
                Arc::new(McpTool {
                    client: client.clone(),
                    // Leaked once at startup (bounded): the registry keys on
                    // `&'static str`, and these tools live for the process.
                    exposed_name: Box::leak(exposed.into_boxed_str()),
                    description: Box::leak(d.description.into_boxed_str()),
                    schema: d.schema,
                    remote_name: d.name,
                    read_only: d.read_only,
                }) as Arc<dyn Tool>
            })
            .collect();
        Ok((client, tools))
    }

    /// Send a JSON-RPC request and await the matching response (or timeout).
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.stdin_tx
            .send(req.to_string())
            .map_err(|_| anyhow!("MCP server '{}' is not running", self.server))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("MCP '{}' {method}: {e}", self.server),
            Ok(Err(_)) => bail!("MCP '{}' {method}: request dropped", self.server),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("MCP '{}' {method}: timed out", self.server)
            }
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.stdin_tx.send(msg.to_string());
    }

    async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "hrdr", "version": env!("CARGO_PKG_VERSION") },
        });
        self.request("initialize", params, HANDSHAKE_TIMEOUT)
            .await?;
        self.notify("notifications/initialized", json!({}));
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
            // `readOnlyHint` lets the agent batch it concurrently; default false.
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

    /// Invoke a tool by its MCP (un-namespaced) name.
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
    async fn connects_lists_and_calls_a_mock_server() {
        let Some(py) = python() else {
            eprintln!("skipping: no python interpreter");
            return;
        };
        let args = vec!["-u".to_string(), "-c".to_string(), MOCK_SERVER.to_string()];
        let (_client, tools) = McpClient::connect("mock", py, &args, &[])
            .await
            .expect("connect + handshake + tools/list");

        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name(), "mock_echo"); // namespaced <server>_<tool>
        assert!(tool.read_only()); // from readOnlyHint
        assert!(tool.description().contains("Echo"));

        let ctx = ToolContext::new(".");
        let out = tool
            .execute(json!({ "text": "hi there" }), &ctx)
            .await
            .expect("tools/call");
        assert_eq!(out, "echo: hi there");
    }
}

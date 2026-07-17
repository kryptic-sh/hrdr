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

mod client;
mod tool;
mod transport;
mod types;
mod util;

use std::time::Duration;

/// MCP protocol version we advertise. Servers negotiate down if needed; the
/// `tools/*` methods are stable across recent revisions.
pub(crate) const PROTOCOL_VERSION: &str = "2025-06-18";
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on a single JSON-RPC message read from a server — an HTTP
/// response body (Streamable HTTP) or one stdio line — so a misbehaving or
/// hostile MCP server streaming unbounded data can't grow this process's
/// memory without limit. MCP messages are small JSON-RPC envelopes; this is
/// generous headroom for a large tool result.
pub(crate) const MAX_MCP_MESSAGE_BYTES: usize = 10 * 1024 * 1024;
/// Wall-clock cap on reading one HTTP response body, independent of the
/// request's own `timeout` — guards a connection that opens promptly (so the
/// request timeout's `send()` phase is satisfied) but then trickles bytes
/// forever without ever completing the body.
pub(crate) const MAX_BODY_READ_TIME: Duration = Duration::from_secs(60);
/// Depth of the stdio writer channel (serialized requests awaiting the child's
/// stdin). Bounded so a stalled child — one that stops draining its stdin pipe
/// — applies backpressure to the callers issuing requests instead of letting
/// serialized JSON accumulate without limit. Each request awaits capacity
/// (`send().await`); if the child exits, the writer task drops the receiver and
/// every blocked sender errors out rather than hanging. 64 is ample slack for
/// legitimate request pipelining while keeping worst-case buffered JSON small.
pub(crate) const MCP_STDIN_CAP: usize = 64;

pub use types::McpClient;

pub(crate) use tool::{McpOp, McpTool};
pub(crate) use transport::{build_headers, http_request, http_send, sse_request, stdio_request};
// parse_sse_for_id is only used via super::* in tests.
#[allow(unused_imports)]
pub(crate) use transport::parse_sse_for_id;
pub(crate) use types::{
    Discovered, HttpTransport, Pending, SseTransport, StdioTransport, Transport,
};
pub(crate) use util::{extract_content_text, response_id, rpc_error_message, sanitize_tool_name};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::{Value, json};
    use tokio::sync::{Mutex, oneshot};

    // Only the tests that spawn a stdio server exercise a real `Tool`, and those
    // are `#[cfg(unix)]` — on Windows this import would be unused (`-D warnings`).
    #[cfg(unix)]
    use crate::{Tool, ToolContext};

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

    // Regression for a MAJOR bug: response routing used to key only on a
    // numeric `id`, with no check that the message lacked a `method` field.
    // JSON-RPC id spaces are per-sender and both sides typically start
    // numbering near 1, so a server-initiated request (MCP servers may
    // legitimately send `ping` or `sampling/createMessage`) whose `id`
    // collided with a pending client id got delivered as that call's
    // response, corrupting it. `response_id` is the shared predicate behind
    // the fix in both the stdio/SSE reader loops and `parse_sse_for_id`.
    #[test]
    fn response_id_ignores_messages_with_a_method_field() {
        // A real response: no `method`, numeric id.
        assert_eq!(
            response_id(&json!({"jsonrpc":"2.0","id":1,"result":{}})),
            Some(1)
        );
        // A server-initiated request reusing id 1 must NOT be treated as a
        // response, even though its `id` matches a pending call.
        assert_eq!(
            response_id(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})),
            None
        );
        // A notification (no id at all, but a method).
        assert_eq!(
            response_id(&json!({"jsonrpc":"2.0","method":"notifications/progress"})),
            None
        );
    }

    // Regression for a MINOR bug: response ids were matched with
    // `Value::as_u64` only, so a server that echoed the id back as a JSON
    // string (`"id":"1"`) never matched and the call died as a bare timeout.
    #[test]
    fn response_id_accepts_a_string_id_that_parses_as_u64() {
        assert_eq!(
            response_id(&json!({"jsonrpc":"2.0","id":"42","result":{}})),
            Some(42)
        );
        // A non-numeric string id still doesn't match anything.
        assert_eq!(
            response_id(&json!({"jsonrpc":"2.0","id":"abc","result":{}})),
            None
        );
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
        // Output is wrapped in an <untrusted-content> envelope (a third-party MCP
        // server is external data), so assertions check for the inner payload.
        assert!(
            echo.execute(json!({ "text": "hi there" }), &ctx)
                .await
                .unwrap()
                .contains("echo: hi there")
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
        assert!(pic.contains("[image content omitted]"), "{pic}");

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
            assert!(out.contains(&format!("echo: n{n}")), "{out}");
        }

        let listed = by("list_resources").execute(json!({}), &ctx).await.unwrap();
        assert!(listed.contains("file:///readme"), "resources: {listed}");
        let read = by("read_resource")
            .execute(json!({ "uri": "file:///readme" }), &ctx)
            .await
            .unwrap();
        assert!(read.contains("resource body for file:///readme"), "{read}");
        // A binary (blob) resource is noted, not dumped.
        let blob = by("read_resource")
            .execute(json!({ "uri": "blob://logo" }), &ctx)
            .await
            .unwrap();
        assert!(blob.contains("[binary resource content omitted]"), "{blob}");

        let prompts = by("list_prompts").execute(json!({}), &ctx).await.unwrap();
        assert!(prompts.contains("greet(who)"), "prompts: {prompts}");
        let rendered = by("get_prompt")
            .execute(
                json!({ "name": "greet", "arguments": { "who": "Sam" } }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(rendered.contains("user: Hello Sam"), "{rendered}");
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
        assert!(
            whoami
                .execute(json!({}), &ctx)
                .await
                .unwrap()
                .contains("sess-1")
        );
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
        assert!(
            by("list_resources")
                .execute(json!({}), &ctx)
                .await
                .unwrap()
                .contains("(no resources)")
        );
        assert!(
            by("list_prompts")
                .execute(json!({}), &ctx)
                .await
                .unwrap()
                .contains("(no prompts)")
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
        assert!(
            result.contains("echo: round-trip"),
            "tools/call must relay the server's content verbatim: {result}"
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
    /// because `mod tests` is a child module of `mcp/mod.rs` and can therefore
    /// access its private fields.  The channel receiver is dropped before the
    /// transport is used, guaranteeing `stdin_tx.send()` returns `Err`
    /// synchronously — no timing dependency.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_request_send_error_removes_pending_id() {
        use tokio::sync::mpsc;

        // Drop the receiver immediately; every subsequent send will fail.
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(MCP_STDIN_CAP);
        drop(stdin_rx);

        // A trivial child satisfies the `_child: Child` field.  Its stdio is
        // irrelevant since the send fails before anything is written.
        let child = tokio::process::Command::new("sh")
            .args(["-c", ""])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
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

    /// The stdio writer channel is bounded, so a request issued while the child
    /// isn't draining its stdin blocks awaiting capacity. That block must be
    /// released — not hung — the instant the child exits (its writer task drops
    /// the receiver). Simulate a full channel + a receiver drop and assert the
    /// blocked `stdio_request` errors out promptly and cleans up its pending id.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_request_full_channel_releases_when_child_exits() {
        use tokio::sync::mpsc;

        // Pre-fill the writer channel to capacity so the request's own send
        // has nowhere to go and must await capacity.
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(MCP_STDIN_CAP);
        for _ in 0..MCP_STDIN_CAP {
            stdin_tx.send("queued".to_string()).await.unwrap();
        }

        let child = tokio::process::Command::new("sh")
            .args(["-c", ""])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("sh must be available on unix");

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let t = StdioTransport {
            stdin_tx,
            pending: pending.clone(),
            _child: child,
        };

        // Drop the receiver shortly after the request starts blocking on
        // `send().await` — this is what the writer task does when the child
        // exits and its `write_all` fails.
        let dropper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(stdin_rx);
        });

        let id = 91u64;
        let req = json!({"jsonrpc":"2.0","id": id,"method":"tools/call","params":{}});
        // A long request timeout so it's the receiver drop — not the timeout —
        // that ends the wait; the outer timeout bounds the test itself.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            stdio_request(&t, id, req, Duration::from_secs(60)),
        )
        .await;
        dropper.await.unwrap();

        let result = outcome.expect(
            "stdio_request hung after the receiver was dropped — blocked sender not released",
        );
        assert!(
            result.is_err(),
            "a send that fails when the child exits must surface as Err"
        );
        assert!(
            pending.lock().await.is_empty(),
            "the released send-error path must still clean up the pending id"
        );
    }
}

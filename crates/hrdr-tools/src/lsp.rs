//! Post-edit LSP diagnostics: after `edit`/`write`/`patch`/`replace` mutate a
//! file, the matching language server checks it and any **errors** ride back
//! to the model appended to the tool result — a wrong edit is caught in the
//! same round it was made, instead of at the next build.
//!
//! This is a deliberately small LSP client: spawn the server for the file's
//! language on first use (presence-aware — a server that isn't installed is a
//! silent no-op, like `grep`'s backend fallback), `initialize`, then per
//! mutation `didOpen`/`didChange` with the new content and wait (bounded) for
//! `textDocument/publishDiagnostics`. Servers stay warm for the session, keyed
//! by command. Anything unexpected — a dead server, a timeout, a malformed
//! frame — degrades to "no diagnostics", never to a failed edit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// How long `initialize` may take before the server is written off.
const INIT_TIMEOUT_MS: u64 = 15_000;
/// Default per-edit wait for `publishDiagnostics` (config `[lsp] wait_ms`).
pub const DEFAULT_LSP_WAIT_MS: u64 = 2_000;
/// After the first publish for the file, wait this much longer for a
/// follow-up — servers often publish a quick stale/empty set first and the
/// real analysis a beat later.
const SETTLE_MS: u64 = 300;
/// Diagnostics lines shown per edit before "…and N more".
const MAX_DIAG_LINES: usize = 8;

/// One language server: which command to run and which files it checks.
#[derive(Debug, Clone)]
pub struct LspServerConfig {
    /// Executable (must be on PATH; checked at first use).
    pub command: String,
    pub args: Vec<String>,
    /// File extensions (lowercase, no dot) routed to this server.
    pub extensions: Vec<String>,
}

/// The built-in, presence-aware server registry: a server is only ever
/// spawned if its binary is on PATH, so this costs nothing to list. Custom
/// `[[lsp.servers]]` config entries are consulted first and win for their
/// extensions.
pub fn default_lsp_servers() -> Vec<LspServerConfig> {
    let s = |command: &str, args: &[&str], extensions: &[&str]| LspServerConfig {
        command: command.to_string(),
        args: args.iter().map(ToString::to_string).collect(),
        extensions: extensions.iter().map(ToString::to_string).collect(),
    };
    vec![
        s("rust-analyzer", &[], &["rs"]),
        s(
            "typescript-language-server",
            &["--stdio"],
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        ),
        s("pyright-langserver", &["--stdio"], &["py", "pyi"]),
        s("gopls", &[], &["go"]),
        s("clangd", &[], &["c", "h", "cpp", "hpp", "cc", "hh", "cxx"]),
    ]
}

/// The `languageId` for a `didOpen`, from the file extension. Falls back to
/// the extension itself, which most servers accept for their own files.
fn language_id(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "hh" | "cxx" => "cpp",
        other => other,
    }
}

/// A `file://` URI for `path` (absolute), with the few characters that break
/// URIs percent-encoded.
fn file_uri(path: &Path) -> String {
    let p = path.display().to_string().replace('\\', "/");
    let mut escaped = String::with_capacity(p.len());
    for c in p.chars() {
        match c {
            ' ' => escaped.push_str("%20"),
            '%' => escaped.push_str("%25"),
            '#' => escaped.push_str("%23"),
            '?' => escaped.push_str("%3F"),
            _ => escaped.push(c),
        }
    }
    if escaped.starts_with('/') {
        format!("file://{escaped}")
    } else {
        // Windows drive path (`C:/…`) needs the extra slash.
        format!("file:///{escaped}")
    }
}

/// The session's language servers, spawned lazily per command and kept warm.
/// Lives in [`crate::ToolContext`] (`ctx.lsp`), shared by every mutating tool.
pub struct LspRegistry {
    root: PathBuf,
    configs: Vec<LspServerConfig>,
    /// Per-edit wait for diagnostics.
    wait_ms: u64,
    /// Command → running client, or `None` when it failed/isn't installed
    /// (remembered so an absent server isn't re-probed on every edit).
    clients: tokio::sync::Mutex<HashMap<String, Option<Arc<LspClient>>>>,
}

impl LspRegistry {
    /// `configs` are consulted in order (put custom servers first); `root` is
    /// the workspace the servers are initialized against.
    pub fn new(root: PathBuf, configs: Vec<LspServerConfig>, wait_ms: Option<u64>) -> Self {
        Self {
            root,
            configs,
            wait_ms: wait_ms.unwrap_or(DEFAULT_LSP_WAIT_MS),
            clients: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Check `path` (now holding `content`) with its language's server and
    /// format any **errors** as a note for the tool result. `None` when the
    /// file has no server, the server isn't installed, nothing arrived in
    /// time, or the file is clean.
    pub async fn diagnostics_note(&self, path: &Path, content: &str) -> Option<String> {
        // Only files inside the workspace the servers were initialized
        // against. A worktree-isolated sub-agent's tree (or a temp-dir
        // scratch file) sits outside the servers' rootUri, where diagnostics
        // are server-dependent — some analyze, some return nothing, some
        // complain about a file "not in the workspace". Skipping is the
        // deliberate, uniform behavior.
        if !path.starts_with(&self.root) {
            return None;
        }
        let ext = path.extension()?.to_string_lossy().to_lowercase();
        let config = self
            .configs
            .iter()
            .find(|c| c.extensions.iter().any(|e| e == &ext))?;
        let client = self.client_for(config).await?;
        let diags = client
            .check_file(path, &ext, content, Duration::from_millis(self.wait_ms))
            .await;
        format_diagnostics(&self.root, path, &diags)
    }

    /// The running client for `config`, spawning it on first use. `None` when
    /// the binary is absent or it failed to start (cached — no re-probing).
    async fn client_for(&self, config: &LspServerConfig) -> Option<Arc<LspClient>> {
        let mut clients = self.clients.lock().await;
        if let Some(known) = clients.get(&config.command) {
            return known.clone();
        }
        let started = if which::which(&config.command).is_ok() {
            LspClient::start(config, &self.root).await.ok()
        } else {
            None
        };
        clients.insert(config.command.clone(), started.clone());
        started
    }
}

/// The latest `publishDiagnostics` per URI: `(version, diagnostics)`.
type DiagnosticsByUri = HashMap<String, (Option<i64>, Vec<Value>)>;

/// One running language server: the JSON-RPC plumbing plus the diagnostics
/// mailbox the reader task fills.
struct LspClient {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    next_id: AtomicI64,
    /// In-flight requests awaiting a response, by id.
    pending: std::sync::Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>,
    diags: std::sync::Mutex<DiagnosticsByUri>,
    /// Pinged by the reader on every publish, so waiters wake immediately.
    diag_notify: tokio::sync::Notify,
    /// Documents already opened on the server: URI → version.
    open_docs: tokio::sync::Mutex<HashMap<String, i64>>,
    /// Owns the process; `kill_on_drop` reaps it when the registry drops.
    _child: tokio::process::Child,
}

impl LspClient {
    /// Spawn + `initialize` + `initialized`.
    async fn start(config: &LspServerConfig, root: &Path) -> Result<Arc<Self>> {
        let mut child = tokio::process::Command::new(&config.command)
            .args(&config.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", config.command))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: std::sync::Mutex::new(HashMap::new()),
            diags: std::sync::Mutex::new(HashMap::new()),
            diag_notify: tokio::sync::Notify::new(),
            open_docs: tokio::sync::Mutex::new(HashMap::new()),
            _child: child,
        });
        tokio::spawn(Self::read_loop(Arc::clone(&client), stdout));

        let root_uri = file_uri(root);
        let init = client
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
                    "capabilities": {
                        "textDocument": {
                            "synchronization": {},
                            "publishDiagnostics": {"versionSupport": true},
                        },
                        "workspace": {"configuration": true},
                    },
                }),
                Duration::from_millis(INIT_TIMEOUT_MS),
            )
            .await?;
        drop(init);
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    /// The reader half: parse framed messages forever, filing responses to
    /// their waiters, answering server→client requests (with the emptiest
    /// legal answer), and dropping diagnostics into the mailbox.
    async fn read_loop(client: Arc<Self>, stdout: tokio::process::ChildStdout) {
        let mut reader = BufReader::new(stdout);
        loop {
            let Ok(msg) = read_frame(&mut reader).await else {
                return; // EOF / server died: pending waiters time out
            };
            let id = msg.get("id").and_then(Value::as_i64);
            let method = msg.get("method").and_then(Value::as_str);
            match (id, method) {
                // A response to one of our requests.
                (Some(id), None) => {
                    let waiter = client.pending.lock().ok().and_then(|mut p| p.remove(&id));
                    if let Some(tx) = waiter {
                        let _ = tx.send(msg.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                // A server→client request: answer minimally so it proceeds.
                (Some(id), Some(method)) => {
                    let result = match method {
                        // One `null` per requested configuration item.
                        "workspace/configuration" => {
                            let n = msg
                                .pointer("/params/items")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len);
                            Value::Array(vec![Value::Null; n])
                        }
                        _ => Value::Null,
                    };
                    let _ = client
                        .send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
                        .await;
                }
                // A notification; diagnostics are the only one we consume.
                (None, Some("textDocument/publishDiagnostics")) => {
                    let uri = msg
                        .pointer("/params/uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let version = msg.pointer("/params/version").and_then(Value::as_i64);
                    let list = msg
                        .pointer("/params/diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if let Ok(mut d) = client.diags.lock() {
                        d.insert(uri, (version, list));
                    }
                    client.diag_notify.notify_waiters();
                }
                _ => {}
            }
        }
    }

    /// Sync the file to the server (`didOpen` first time, `didChange` after)
    /// and wait — bounded by `wait` — for its diagnostics.
    async fn check_file(
        &self,
        path: &Path,
        ext: &str,
        content: &str,
        wait: Duration,
    ) -> Vec<Value> {
        let uri = file_uri(path);
        // Forget the previous publish so an old result can't answer for this
        // edit (a same-version republish is indistinguishable otherwise).
        if let Ok(mut d) = self.diags.lock() {
            d.remove(&uri);
        }
        let mut open = self.open_docs.lock().await;
        let version = match open.get(&uri) {
            Some(v) => {
                let v = v + 1;
                open.insert(uri.clone(), v);
                let sent = self
                    .notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": {"uri": uri, "version": v},
                            "contentChanges": [{"text": content}],
                        }),
                    )
                    .await;
                if sent.is_err() {
                    return Vec::new();
                }
                v
            }
            None => {
                open.insert(uri.clone(), 1);
                let sent = self
                    .notify(
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": language_id(ext),
                                "version": 1,
                                "text": content,
                            },
                        }),
                    )
                    .await;
                if sent.is_err() {
                    return Vec::new();
                }
                1
            }
        };
        drop(open);

        // Wait for a publish for this URI. Servers often publish a quick
        // (stale or empty) set and the real analysis shortly after, so once
        // one arrives, allow a short settle window for a follow-up. A publish
        // stamped with an older version is ignored outright.
        let deadline = tokio::time::Instant::now() + wait;
        let mut settle_until: Option<tokio::time::Instant> = None;
        loop {
            let current = self.diags.lock().ok().and_then(|d| {
                d.get(&uri)
                    .filter(|(v, _)| v.is_none() || *v == Some(version))
                    .map(|(_, list)| list.clone())
            });
            let now = tokio::time::Instant::now();
            if let Some(list) = current {
                if settle_until.is_none() {
                    settle_until = Some((now + Duration::from_millis(SETTLE_MS)).min(deadline));
                }
                if now >= settle_until.unwrap() || !list.is_empty() {
                    return list;
                }
            }
            let next = settle_until.unwrap_or(deadline).min(deadline);
            if now >= next {
                return Vec::new();
            }
            let _ = tokio::time::timeout_at(next, self.diag_notify.notified()).await;
        }
    }

    /// Send a request and await its response.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let res = tokio::time::timeout(timeout, rx)
            .await
            .with_context(|| format!("{method} timed out"))?;
        res.map_err(|_| anyhow::anyhow!("{method}: server closed"))
    }

    /// Send a notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Write one framed message.
    async fn send(&self, msg: &Value) -> Result<()> {
        let body = msg.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(frame.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// Read one `Content-Length`-framed JSON-RPC message.
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            anyhow::bail!("eof");
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            length = v.trim().parse().ok();
        }
    }
    let length = length.context("no Content-Length header")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Format the **errors** among `diags` as one tool-result note. Warnings and
/// hints are deliberately dropped — they'd bury the signal on lint-heavy
/// codebases; the model gets what would actually break the build.
fn format_diagnostics(root: &Path, path: &Path, diags: &[Value]) -> Option<String> {
    let errors: Vec<&Value> = diags
        .iter()
        // Severity 1 = Error; a missing severity is treated as an error per
        // the LSP spec's "up to the client" default.
        .filter(|d| d.get("severity").and_then(Value::as_i64).unwrap_or(1) == 1)
        .collect();
    if errors.is_empty() {
        return None;
    }
    let rel = path.strip_prefix(root).unwrap_or(path).display();
    let mut lines = Vec::with_capacity(errors.len().min(MAX_DIAG_LINES) + 1);
    lines.push(format!(
        "[lsp] {} error{} in {rel}:",
        errors.len(),
        if errors.len() == 1 { "" } else { "s" }
    ));
    for d in errors.iter().take(MAX_DIAG_LINES) {
        let line = d
            .pointer("/range/start/line")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let col = d
            .pointer("/range/start/character")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let msg = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)")
            .lines()
            .next()
            .unwrap_or_default();
        lines.push(format!("  {rel}:{line}:{col} {msg}"));
    }
    if errors.len() > MAX_DIAG_LINES {
        lines.push(format!("  …and {} more", errors.len() - MAX_DIAG_LINES));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_and_language_ids() {
        assert_eq!(
            file_uri(Path::new("/proj/src/a b.rs")),
            "file:///proj/src/a%20b.rs"
        );
        assert_eq!(language_id("rs"), "rust");
        assert_eq!(language_id("tsx"), "typescript");
        assert_eq!(language_id("zig"), "zig"); // unknown → the extension itself
    }

    #[test]
    fn diagnostics_format_errors_only_and_cap() {
        let root = Path::new("/proj");
        let path = Path::new("/proj/src/main.rs");
        let err = |line: i64, msg: &str| {
            json!({"range": {"start": {"line": line, "character": 4}},
                   "severity": 1, "message": msg})
        };
        let warn = json!({"range": {"start": {"line": 0, "character": 0}},
                          "severity": 2, "message": "unused import"});

        // Warnings alone → clean.
        assert_eq!(
            format_diagnostics(root, path, std::slice::from_ref(&warn)),
            None
        );
        // Errors are listed 1-based, warnings dropped.
        let note =
            format_diagnostics(root, path, &[warn, err(9, "mismatched types\nlong help")]).unwrap();
        assert!(note.contains("1 error in src/main.rs"), "{note}");
        assert!(
            note.contains("src/main.rs:10:5 mismatched types"),
            "first line only, 1-based: {note}"
        );
        // The cap kicks in past MAX_DIAG_LINES.
        let many: Vec<Value> = (0..12).map(|i| err(i, "boom")).collect();
        let note = format_diagnostics(root, path, &many).unwrap();
        assert!(note.contains("12 errors"), "{note}");
        assert!(note.contains("…and 4 more"), "{note}");
    }

    /// Paths outside the registry's root are skipped before any server is
    /// consulted (or spawned): the servers were initialized against the root
    /// workspace, so out-of-root files — a worktree-isolated sub-agent's
    /// tree, temp-dir scratch files — get deliberately-uniform silence
    /// instead of server-dependent behavior.
    #[tokio::test]
    async fn out_of_root_paths_are_skipped() {
        let registry = LspRegistry::new(
            PathBuf::from("/proj"),
            vec![LspServerConfig {
                command: "definitely-missing-lsp-server".to_string(),
                args: vec![],
                extensions: vec!["xyz".to_string()],
            }],
            None,
        );
        assert_eq!(
            registry
                .diagnostics_note(Path::new("/elsewhere/a.xyz"), "boom")
                .await,
            None
        );
    }

    /// The full round-trip against a scripted LSP server (python3): spawn,
    /// initialize, didOpen → publishDiagnostics with an error → formatted
    /// note; a clean follow-up didChange → no note. Skips when python3 is
    /// absent.
    #[cfg(unix)]
    #[tokio::test]
    async fn registry_reports_errors_from_a_real_server_process() {
        if which::which("python3").is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("fake_lsp.py");
        std::fs::write(&server, FAKE_LSP_PY).unwrap();
        let file = dir.path().join("main.xyz");

        let registry = Arc::new(LspRegistry::new(
            dir.path().to_path_buf(),
            vec![LspServerConfig {
                command: "python3".to_string(),
                args: vec![server.display().to_string()],
                extensions: vec!["xyz".to_string()],
            }],
            Some(5_000),
        ));

        // The fake server flags any line containing "boom".
        std::fs::write(&file, "ok\nboom here\n").unwrap();
        let note = registry
            .diagnostics_note(&file, "ok\nboom here\n")
            .await
            .expect("an error note");
        assert!(note.contains("1 error in main.xyz"), "{note}");
        assert!(note.contains("main.xyz:2:1 found boom"), "{note}");

        // Clean content → no note (didChange path, same warm server).
        assert_eq!(registry.diagnostics_note(&file, "all good\n").await, None);

        // No server registered for the extension → silent no-op.
        assert_eq!(
            registry
                .diagnostics_note(Path::new("/tmp/x.nope"), "boom")
                .await,
            None
        );

        // End-to-end through a mutating tool: `write` picks the note up via
        // `apply_file_change` and it rides the tool result to the model.
        use crate::Tool as _;
        let mut ctx = crate::ToolContext::new(dir.path());
        ctx.lsp = Some(registry);
        let result = crate::WriteTool
            .execute(
                serde_json::json!({"path": "fresh.xyz", "content": "boom again\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            result.contains("[lsp] 1 error in fresh.xyz"),
            "the diagnostics note rides the write result: {result}"
        );
    }

    /// A minimal, protocol-correct LSP server: answers `initialize`, and on
    /// `didOpen`/`didChange` publishes one error per line containing "boom"
    /// (with the document's version, exercising the stale-publish guard).
    #[cfg(unix)] // used only by the unix-gated integration test above
    const FAKE_LSP_PY: &str = r#"
import json, sys

def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline().decode()
        if not line or line == "\r\n":
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":")[1].strip())
    if length is None:
        sys.exit(0)
    return json.loads(sys.stdin.buffer.read(length))

def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

def publish(uri, version, text):
    diags = []
    for i, line in enumerate(text.splitlines()):
        if "boom" in line:
            diags.append({
                "range": {"start": {"line": i, "character": 0},
                          "end": {"line": i, "character": 1}},
                "severity": 1,
                "message": "found boom",
            })
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "version": version, "diagnostics": diags}})

while True:
    msg = read()
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        d = msg["params"]["textDocument"]
        publish(d["uri"], d["version"], d["text"])
    elif method == "textDocument/didChange":
        d = msg["params"]["textDocument"]
        publish(d["uri"], d["version"], msg["params"]["contentChanges"][0]["text"])
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
"#;
}

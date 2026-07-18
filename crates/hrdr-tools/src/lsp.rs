//! Post-edit LSP diagnostics: after `edit`/`write`/`replace` mutate a
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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
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
/// How long a navigation request (`definition`/`references`/`rename`) may
/// take — longer than the diagnostics wait: the model asked explicitly, and
/// an indexing server answers when ready.
const NAV_TIMEOUT_MS: u64 = 30_000;

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
/// URIs percent-encoded — including, per RFC 3986 (URIs are ASCII-only), every
/// non-ASCII byte of a multi-byte UTF-8 character. Without that, a filename
/// like `café.rs` would ride into the URI as raw UTF-8 bytes rather than
/// `%C3%A9`, which doesn't match the percent-encoded spelling a language
/// server hands back in its own URIs — so a diagnostics lookup keyed by this
/// URI (see `check_file`) would miss, and [`uri_to_path`] round-tripping ours
/// would have nothing to decode.
fn file_uri(path: &Path) -> String {
    let p = path.display().to_string().replace('\\', "/");
    let mut escaped = String::with_capacity(p.len());
    for byte in p.bytes() {
        match byte {
            b' ' => escaped.push_str("%20"),
            b'%' => escaped.push_str("%25"),
            b'#' => escaped.push_str("%23"),
            b'?' => escaped.push_str("%3F"),
            0x80..=0xFF => escaped.push_str(&format!("%{byte:02X}")),
            _ => escaped.push(byte as char),
        }
    }
    if escaped.starts_with('/') {
        format!("file://{escaped}")
    } else {
        // Windows drive path (`C:/…`) needs the extra slash.
        format!("file:///{escaped}")
    }
}

/// A configured server's lifecycle state (for `/doctor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspServerStatus {
    /// No matching file has been touched yet, so it hasn't been probed.
    NotYetUsed,
    /// Spawned and initialized this session.
    Running,
    /// Probed and the binary isn't on PATH.
    NotInstalled,
    /// The binary exists but spawn/initialize failed.
    Failed,
}

impl LspServerStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotYetUsed => "not yet used",
            Self::Running => "running",
            Self::NotInstalled => "not installed",
            Self::Failed => "failed to start",
        }
    }
}

/// One row of [`LspRegistry::statuses`].
#[derive(Debug, Clone)]
pub struct LspServerReport {
    pub command: String,
    pub extensions: Vec<String>,
    pub status: LspServerStatus,
}

/// A probed server slot: running, or remembered-unavailable (so an absent
/// server isn't re-probed on every edit).
enum ClientSlot {
    Running(Arc<LspClient>),
    Unavailable(LspServerStatus),
}

/// The session's language servers, spawned lazily per command and kept warm.
/// Lives in [`crate::ToolContext`] (`ctx.lsp`), shared by every mutating tool.
pub struct LspRegistry {
    root: PathBuf,
    configs: Vec<LspServerConfig>,
    /// Per-edit wait for diagnostics.
    wait_ms: u64,
    /// Command → probed slot; absent = not yet used.
    clients: tokio::sync::Mutex<HashMap<String, ClientSlot>>,
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
        let wait = Duration::from_millis(self.wait_ms);
        // Bound the whole sync + wait interaction. A wedged server — one that
        // stopped draining its stdin (a crashed-but-not-exited process, a
        // rust-analyzer stuck on a huge crate) — must not hang the edit. `send`
        // already caps each write, but wrap the collection too so a stall
        // degrades to "no diagnostics" (best-effort, the edit still succeeds)
        // instead of a stuck tool call, and retire the server so later edits
        // skip it fast.
        let diags = match tokio::time::timeout(
            wait + client.send_timeout + Duration::from_millis(500),
            client.check_file(path, &ext, content, wait),
        )
        .await
        {
            Ok(diags) => diags,
            Err(_) => {
                client.mark_degraded();
                return None;
            }
        };
        format_diagnostics(&self.root, path, &diags)
    }

    /// The running client for `config`, spawning it on first use. `None` when
    /// the binary is absent or it failed to start (cached — no re-probing).
    async fn client_for(&self, config: &LspServerConfig) -> Option<Arc<LspClient>> {
        let mut clients = self.clients.lock().await;
        match clients.get(&config.command) {
            Some(ClientSlot::Running(c)) => {
                if c.is_degraded() {
                    // A prior call wedged this server (a write timed out on a
                    // stalled stdin). Retire the slot so it's never handed out
                    // again and later edits skip it fast instead of each
                    // re-hitting the write timeout.
                    clients.insert(
                        config.command.clone(),
                        ClientSlot::Unavailable(LspServerStatus::Failed),
                    );
                    return None;
                }
                return Some(Arc::clone(c));
            }
            Some(ClientSlot::Unavailable(_)) => return None,
            None => {}
        }
        let slot = if which::which(&config.command).is_err() {
            ClientSlot::Unavailable(LspServerStatus::NotInstalled)
        } else {
            match LspClient::start(config, &self.root, self.wait_ms).await {
                Ok(c) => ClientSlot::Running(c),
                Err(_) => ClientSlot::Unavailable(LspServerStatus::Failed),
            }
        };
        let started = match &slot {
            ClientSlot::Running(c) => Some(Arc::clone(c)),
            ClientSlot::Unavailable(_) => None,
        };
        clients.insert(config.command.clone(), slot);
        started
    }

    /// One status row per configured server, in config order (for `/doctor`).
    pub async fn statuses(&self) -> Vec<LspServerReport> {
        let clients = self.clients.lock().await;
        self.configs
            .iter()
            .map(|c| LspServerReport {
                command: c.command.clone(),
                extensions: c.extensions.clone(),
                status: match clients.get(&c.command) {
                    None => LspServerStatus::NotYetUsed,
                    Some(ClientSlot::Running(client)) => {
                        if client.is_degraded() {
                            // Spawned fine, but a later write wedged it.
                            LspServerStatus::Failed
                        } else {
                            LspServerStatus::Running
                        }
                    }
                    Some(ClientSlot::Unavailable(s)) => *s,
                },
            })
            .collect()
    }

    /// The configured per-edit diagnostics wait (for `/doctor`).
    pub fn wait_ms(&self) -> u64 {
        self.wait_ms
    }

    /// The client + extension for a navigation request on `path` — unlike the
    /// diagnostics path, absence is an error here (the model called a tool and
    /// deserves to know why nothing came back).
    async fn nav_client(&self, path: &Path) -> Result<(Arc<LspClient>, String)> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let config = self
            .configs
            .iter()
            .find(|c| c.extensions.iter().any(|e| e == &ext))
            .ok_or_else(|| anyhow::anyhow!("no language server is configured for .{ext} files"))?;
        let command = config.command.clone();
        let client = self
            .client_for(config)
            .await
            .ok_or_else(|| anyhow::anyhow!("the {command} language server is not available"))?;
        Ok((client, ext))
    }

    /// The workspace root the servers were initialized against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// One position-based navigation request (`textDocument/definition`,
    /// `…/references`, `…/rename`) for the symbol at `position` —
    /// 0-based `(line, character)`, `character` in UTF-16 code units, per
    /// LSP. `content` is the file's current text, synced to the server first.
    /// `extra` params are merged in (e.g. `newName`). Gated on the server's
    /// advertised `<capability>Provider`.
    pub async fn nav_request(
        &self,
        method: &str,
        capability: &str,
        path: &Path,
        content: &str,
        position: (u32, u32),
        extra: Value,
    ) -> Result<Value> {
        let (client, ext) = self.nav_client(path).await?;
        if !client.supports(capability) {
            anyhow::bail!("this language server does not support {method}");
        }
        let uri = file_uri(path);
        client.sync_file(&uri, &ext, content).await?;
        let mut params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": position.0, "character": position.1},
        });
        if let (Some(obj), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        client
            .request(method, params, Duration::from_millis(NAV_TIMEOUT_MS))
            .await
    }

    /// Spawn + initialize the servers for `extensions` now instead of on the
    /// first edit, so indexing-heavy servers (rust-analyzer) overlap their
    /// warm-up with the session's first prompt rather than its first edit.
    /// Presence-aware and silent like the lazy path — this is just
    /// [`Self::client_for`] called early; results land in the same cache.
    pub async fn pre_warm(&self, extensions: &[String]) {
        for ext in extensions {
            let Some(config) = self
                .configs
                .iter()
                .find(|c| c.extensions.iter().any(|e| e == ext))
            else {
                continue;
            };
            let _ = self.client_for(config).await;
        }
    }
}

/// The latest `publishDiagnostics` per URI: `(version, diagnostics)`.
type DiagnosticsByUri = HashMap<String, (Option<i64>, Vec<Value>)>;

/// One running language server: the JSON-RPC plumbing plus the diagnostics
/// mailbox the reader task fills.
struct LspClient {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    next_id: AtomicI64,
    /// In-flight requests awaiting a response, by id. `Err(message)` carries a
    /// JSON-RPC `error.message` through to the waiter (see [`Self::request`])
    /// instead of the response being silently treated as an empty success.
    pending: std::sync::Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Result<Value, String>>>>,
    diags: std::sync::Mutex<DiagnosticsByUri>,
    /// Pinged by the reader on every publish, so waiters wake immediately.
    diag_notify: tokio::sync::Notify,
    /// Documents already opened on the server: URI → version.
    open_docs: tokio::sync::Mutex<HashMap<String, i64>>,
    /// The server's advertised capabilities (from the `initialize` result) —
    /// gates the navigation requests.
    capabilities: std::sync::OnceLock<Value>,
    /// Set once a write timed out on this server's stdin (it stopped draining):
    /// every later `send` fails fast and the registry retires the client, so a
    /// wedged server can't hang each edit for the full timeout.
    degraded: AtomicBool,
    /// Per-write budget: a single framed message must reach the server's stdin
    /// within this long or the server is treated as wedged. Derived from the
    /// registry's diagnostics wait budget.
    send_timeout: Duration,
    /// Owns the process; `kill_on_drop` reaps it when the registry drops.
    _child: tokio::process::Child,
    /// Owns the process group / job object `_child` was placed in. Declared
    /// after `_child` so it drops after it. Its `Drop` group-kills the server
    /// *and any workers it forked* when the registry (and this `Arc`) goes away
    /// — unix `kill(-pgid)`, Windows job-handle close — not just the leader
    /// `_child`'s `kill_on_drop` reaps.
    _group: Option<crate::proc::ProcessGroup>,
}

impl LspClient {
    /// Spawn + `initialize` + `initialized`.
    async fn start(config: &LspServerConfig, root: &Path, wait_ms: u64) -> Result<Arc<Self>> {
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Own process group / job object: an LSP server (`typescript-language-server`,
        // `rust-analyzer`, …) can itself spawn worker/build processes. This client
        // tears the server down only via drop (the registry going away), and the
        // `ProcessGroup` guard's `Drop` covers that on both platforms — unix
        // signals `kill(-pgid)`, Windows closes the job handle
        // (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) — so a forked worker isn't leaked
        // when the session ends.
        crate::proc::configure(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", config.command))?;
        let group = crate::proc::ProcessGroup::attach(&child).ok();
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: std::sync::Mutex::new(HashMap::new()),
            diags: std::sync::Mutex::new(HashMap::new()),
            diag_notify: tokio::sync::Notify::new(),
            open_docs: tokio::sync::Mutex::new(HashMap::new()),
            capabilities: std::sync::OnceLock::new(),
            degraded: AtomicBool::new(false),
            send_timeout: Duration::from_millis(wait_ms),
            _child: child,
            _group: group,
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
        let _ = client
            .capabilities
            .set(init.get("capabilities").cloned().unwrap_or(Value::Null));
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    /// Whether the server advertised `<name>Provider` in its capabilities
    /// (either `true` or an options object counts).
    fn supports(&self, provider_cap: &str) -> bool {
        self.capabilities
            .get()
            .and_then(|c| c.get(provider_cap))
            .is_some_and(|v| v.as_bool().unwrap_or(v.is_object()))
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
                // A response to one of our requests: forward its `error`
                // (e.g. "cannot rename this symbol") rather than discarding
                // it and leaving only `result` (absent on an error response),
                // which used to surface as a misleading empty "no edits" /
                // "no definition" instead of the server's actual reason.
                (Some(id), None) => {
                    let waiter = client.pending.lock().ok().and_then(|mut p| p.remove(&id));
                    if let Some(tx) = waiter {
                        let outcome = match msg.get("error") {
                            Some(err) => Err(err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                                .to_string()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(outcome);
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

    /// Sync the file's content to the server: `didOpen` on first touch,
    /// `didChange` (full text) after. Returns the document version sent.
    async fn sync_file(&self, uri: &str, ext: &str, content: &str) -> Result<i64> {
        let mut open = self.open_docs.lock().await;
        match open.get(uri) {
            Some(v) => {
                let v = v + 1;
                open.insert(uri.to_string(), v);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": v},
                        "contentChanges": [{"text": content}],
                    }),
                )
                .await?;
                Ok(v)
            }
            None => {
                open.insert(uri.to_string(), 1);
                self.notify(
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
                .await?;
                Ok(1)
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
        let Ok(version) = self.sync_file(&uri, ext, content).await else {
            return Vec::new();
        };

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
        if let Err(e) = self
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
        {
            // The write never went out; don't leave a waiter behind.
            self.drop_pending(id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            // The timeout's `Ok` wraps the channel recv, whose own `Ok` wraps
            // what `read_loop` sent: `Ok(value)` for a normal response,
            // `Err(message)` for a JSON-RPC `error` response.
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => anyhow::bail!("{method} failed: {message}"),
            Ok(Err(_)) => Err(anyhow::anyhow!("{method}: server closed")),
            // Timed out waiting for the response: remove the pending entry so it
            // doesn't leak (a late response finds no waiter and is dropped).
            // Mirrors `mcp::transport::request_via_pending`'s cleanup.
            Err(_) => {
                self.drop_pending(id);
                anyhow::bail!("{method} timed out")
            }
        }
    }

    /// Remove an in-flight request's waiter (on send failure or timeout).
    fn drop_pending(&self, id: i64) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&id);
        }
    }

    /// Whether a previous write wedged this server (stdin stopped draining).
    fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Mark the server unusable so later calls skip it fast.
    fn mark_degraded(&self) {
        self.degraded.store(true, Ordering::Relaxed);
    }

    /// Send a notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Write one framed message, bounded by [`Self::send_timeout`]. A server
    /// that stopped draining its stdin fills the ~64KB pipe and would block the
    /// write forever; the timeout turns that into an error and marks the client
    /// degraded so the caller (best-effort diagnostics) skips it and later calls
    /// fail fast rather than each hanging.
    async fn send(&self, msg: &Value) -> Result<()> {
        if self.is_degraded() {
            anyhow::bail!("language server is unavailable (a previous write timed out)");
        }
        let body = msg.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut stdin = self.stdin.lock().await;
        let write = async {
            stdin.write_all(frame.as_bytes()).await?;
            stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        };
        match tokio::time::timeout(self.send_timeout, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                self.mark_degraded();
                anyhow::bail!("language server write timed out (stdin not draining)")
            }
        }
    }
}

// ── Navigation-result plumbing (shared by the definition/references/rename
//    tools) ─────────────────────────────────────────────────────────────────

/// A resolved code location, 1-based, ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

/// One text replacement from a rename's `WorkspaceEdit`, 0-based LSP
/// positions (UTF-16 `character`s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTextEdit {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub new_text: String,
}

/// All of one file's edits from a rename.
#[derive(Debug, Clone)]
pub struct LspFileEdits {
    pub path: PathBuf,
    pub edits: Vec<LspTextEdit>,
}

/// A `file://` URI back to a path (reverses [`file_uri`]'s escaping).
///
/// Percent-decodes into a single raw byte buffer for the *whole* string, then
/// UTF-8-decodes that buffer once at the end — rather than decoding
/// byte-by-byte and widening each raw byte straight to a `char`
/// (`bytes[i] as char`), which is a Latin-1 mapping, not UTF-8 decoding: for a
/// non-ASCII filename it split a multi-byte UTF-8 character (whether it
/// arrived percent-encoded, as `file_uri` now always sends it, or — from a
/// server that doesn't bother escaping — as literal UTF-8 bytes already in
/// the string) into separate mis-mapped codepoints, e.g. `file:///proj/café.rs`
/// decoding to `/proj/cafÃ©.rs`. Falls back to a lossy decode (replacement
/// characters, never `None`) if the reconstructed bytes still aren't valid
/// UTF-8, matching this function's previous never-fails-on-a-`file://`-prefix
/// contract.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Windows drive form `file:///C:/…` keeps the path after the third slash.
    let rest = if rest.len() > 2 && rest.as_bytes()[0] == b'/' && rest.as_bytes()[2] == b':' {
        &rest[1..]
    } else {
        rest
    };
    let bytes = rest.as_bytes();
    let mut raw: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16)
        {
            raw.push(b);
            i += 3;
            continue;
        }
        raw.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&raw).into_owned()))
}

/// The UTF-16 column of byte offset `byte_col` within `line` (for building a
/// request position from a byte-indexed symbol match).
pub fn byte_to_utf16_col(line: &str, byte_col: usize) -> u32 {
    line[..byte_col.min(line.len())]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// The byte offset of UTF-16 column `utf16_col` within `line` (clamped to the
/// line's end).
fn utf16_to_byte_col(line: &str, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units >= utf16_col {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    line.len()
}

/// Normalize a definition/references result — `null`, a single `Location`, an
/// array of `Location`s, or an array of `LocationLink`s — into printable
/// locations (1-based).
pub fn parse_locations(result: &Value) -> Result<Vec<LspLocation>> {
    let one = |v: &Value| -> Result<LspLocation> {
        let (uri, range) = if let Some(target) = v.get("targetUri") {
            (
                target
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("targetUri not a string"))?,
                v.get("targetSelectionRange")
                    .or_else(|| v.get("targetRange"))
                    .ok_or_else(|| anyhow::anyhow!("missing target range"))?,
            )
        } else {
            (
                v.get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing uri"))?,
                v.get("range")
                    .ok_or_else(|| anyhow::anyhow!("missing range"))?,
            )
        };
        Ok(LspLocation {
            path: uri_to_path(uri).ok_or_else(|| anyhow::anyhow!("bad uri: {uri}"))?,
            line: u32::try_from(
                range
                    .pointer("/start/line")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("missing start/line"))?,
            )
            .map_err(|_| anyhow::anyhow!("line out of range"))?
                + 1,
            column: u32::try_from(
                range
                    .pointer("/start/character")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("missing start/character"))?,
            )
            .map_err(|_| anyhow::anyhow!("character out of range"))?
                + 1,
        })
    };
    match result {
        Value::Array(items) => items.iter().map(one).collect(),
        Value::Null => Ok(Vec::new()),
        v => Ok(vec![one(v)?]),
    }
}

/// Parse a rename's `WorkspaceEdit` (either the `changes` map or
/// `documentChanges` array form) into per-file edit lists. Errors on file
/// create/rename/delete operations — hrdr applies text edits only.
pub fn parse_workspace_edit(result: &Value, cwd: &Path) -> Result<Vec<LspFileEdits>> {
    let check_confined = |path: &Path| -> Result<()> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !canon.starts_with(cwd) {
            anyhow::bail!("rename target {} is outside the workspace", path.display());
        }
        Ok(())
    };

    fn text_edits(edits: &Value) -> Result<Vec<LspTextEdit>> {
        edits
            .as_array()
            .map(|a| a.iter().map(one_edit).collect::<Result<Vec<_>>>())
            .unwrap_or(Ok(Vec::new()))
    }
    fn one_edit(e: &Value) -> Result<LspTextEdit> {
        Ok(LspTextEdit {
            start: (
                u32::try_from(
                    e.pointer("/range/start/line")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("missing start/line"))?,
                )
                .map_err(|_| anyhow::anyhow!("start line out of range"))?,
                u32::try_from(
                    e.pointer("/range/start/character")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("missing start/character"))?,
                )
                .map_err(|_| anyhow::anyhow!("start character out of range"))?,
            ),
            end: (
                u32::try_from(
                    e.pointer("/range/end/line")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("missing end/line"))?,
                )
                .map_err(|_| anyhow::anyhow!("end line out of range"))?,
                u32::try_from(
                    e.pointer("/range/end/character")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow::anyhow!("missing end/character"))?,
                )
                .map_err(|_| anyhow::anyhow!("end character out of range"))?,
            ),
            new_text: e
                .get("newText")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing newText"))?
                .to_string(),
        })
    }

    let mut out = Vec::new();
    if let Some(changes) = result.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let path = uri_to_path(uri).context("bad uri in WorkspaceEdit")?;
            check_confined(&path)?;
            out.push(LspFileEdits {
                path,
                edits: text_edits(edits)?,
            });
        }
    } else if let Some(doc_changes) = result.get("documentChanges").and_then(Value::as_array) {
        for change in doc_changes {
            if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                anyhow::bail!("rename requires a file {kind} operation, which hrdr doesn't apply");
            }
            let uri = change
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .context("bad documentChanges entry")?;
            let path = uri_to_path(uri).context("bad uri in WorkspaceEdit")?;
            check_confined(&path)?;
            out.push(LspFileEdits {
                path,
                edits: text_edits(change.get("edits").unwrap_or(&Value::Null))?,
            });
        }
    } else if !result.is_null() {
        anyhow::bail!("unrecognized WorkspaceEdit shape");
    }
    out.retain(|f| !f.edits.is_empty());
    Ok(out)
}

/// Apply LSP text edits to `content`. Positions are 0-based lines + UTF-16
/// columns; edits are converted to byte ranges and applied last-first so
/// earlier offsets stay valid. Overlapping edits are an error.
pub fn apply_lsp_edits(content: &str, edits: &[LspTextEdit]) -> Result<String> {
    // Byte offset of each line start (including a virtual line past the end,
    // so an edit ending at the start of the line after the last is valid).
    let mut line_starts = vec![0usize];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_text = |l: usize| -> &str {
        let start = line_starts[l];
        let end = line_starts.get(l + 1).map_or(content.len(), |n| n - 1);
        &content[start..end.max(start)]
    };
    let to_offset = |(line, col): (u32, u32)| -> Result<usize> {
        let l = line as usize;
        if l >= line_starts.len() {
            anyhow::bail!(
                "edit position line {} is past the end of the file",
                line + 1
            );
        }
        Ok(line_starts[l] + utf16_to_byte_col(line_text(l), col))
    };

    let mut spans: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for e in edits {
        spans.push((to_offset(e.start)?, to_offset(e.end)?, &e.new_text));
    }
    spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut prev_start = usize::MAX;
    let mut out = content.to_string();
    for (start, end, text) in spans {
        if end > prev_start || start > end {
            anyhow::bail!("overlapping or inverted edits in the rename");
        }
        prev_start = start;
        out.replace_range(start..end, text);
    }
    Ok(out)
}

const MAX_LSP_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_LSP_HEADER_BYTES: usize = 16 * 1024;
const MAX_LSP_HEADERS: usize = 64;

/// Read one `Content-Length`-framed JSON-RPC message under strict framing caps.
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value> {
    let mut length: Option<usize> = None;
    let mut header_bytes = 0usize;
    for _ in 0..MAX_LSP_HEADERS {
        let mut line = String::new();
        let remaining = MAX_LSP_HEADER_BYTES.saturating_sub(header_bytes);
        if remaining == 0 {
            anyhow::bail!("LSP header exceeds {MAX_LSP_HEADER_BYTES} bytes");
        }
        let read = reader
            .take((remaining + 1) as u64)
            .read_line(&mut line)
            .await?;
        if read == 0 {
            anyhow::bail!("eof");
        }
        header_bytes += read;
        if header_bytes > MAX_LSP_HEADER_BYTES {
            anyhow::bail!("LSP header exceeds {MAX_LSP_HEADER_BYTES} bytes");
        }
        let line = line.trim_end();
        if line.is_empty() {
            let length = length.context("no Content-Length header")?;
            if length > MAX_LSP_FRAME_BYTES {
                anyhow::bail!(
                    "LSP Content-Length {length} exceeds {MAX_LSP_FRAME_BYTES}-byte frame limit"
                );
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).await?;
            return Ok(serde_json::from_slice(&body)?);
        }
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            length = Some(v.trim().parse().context("invalid Content-Length header")?);
        }
    }
    anyhow::bail!("LSP header exceeds {MAX_LSP_HEADERS} lines")
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

    #[tokio::test]
    async fn oversized_lsp_frame_is_rejected_before_body_read() {
        let input = format!("Content-Length: {}\r\n\r\n", MAX_LSP_FRAME_BYTES + 1);
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        let err = read_frame(&mut reader).await.unwrap_err().to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn oversized_lsp_header_is_bounded() {
        let input = format!("X-Test: {}\r\n\r\n", "x".repeat(MAX_LSP_HEADER_BYTES + 1));
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        let err = read_frame(&mut reader).await.unwrap_err().to_string();
        assert!(err.contains("header"), "{err}");
    }

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

    /// `/doctor`'s status rows track the probe lifecycle: unprobed → "not yet
    /// used"; a probe that finds no binary → "not installed" (cached).
    #[tokio::test]
    async fn statuses_track_probe_results() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LspRegistry::new(
            dir.path().to_path_buf(),
            vec![LspServerConfig {
                command: "definitely-missing-lsp-server".to_string(),
                args: vec![],
                extensions: vec!["xyz".to_string()],
            }],
            None,
        );
        assert_eq!(
            registry.statuses().await[0].status,
            LspServerStatus::NotYetUsed
        );
        let _ = registry
            .diagnostics_note(&dir.path().join("a.xyz"), "x")
            .await;
        assert_eq!(
            registry.statuses().await[0].status,
            LspServerStatus::NotInstalled
        );
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

    /// `pre_warm` spawns the matching server without any edit having
    /// happened; unknown extensions are skipped silently.
    #[cfg(unix)]
    #[tokio::test]
    async fn pre_warm_spawns_the_server_before_any_edit() {
        if which::which("python3").is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("fake_lsp.py");
        std::fs::write(&server, FAKE_LSP_PY).unwrap();
        let registry = LspRegistry::new(
            dir.path().to_path_buf(),
            vec![LspServerConfig {
                command: "python3".to_string(),
                args: vec![server.display().to_string()],
                extensions: vec!["xyz".to_string()],
            }],
            Some(5_000),
        );
        registry
            .pre_warm(&["xyz".to_string(), "unknown-ext".to_string()])
            .await;
        assert_eq!(
            registry.statuses().await[0].status,
            LspServerStatus::Running,
            "pre-warm spawned the server"
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
        assert_eq!(
            registry.statuses().await[0].status,
            LspServerStatus::Running,
            "the warm server reports as running"
        );

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

    /// Regression (MINOR): a JSON-RPC `error` response (e.g. `rename`'s
    /// "cannot rename this symbol") must reach the caller as that message —
    /// not be silently discarded so it surfaces as an empty, misleading "no
    /// edits"/"no definition" instead. The fake server here answers every
    /// navigation request with an `error`, never a `result`.
    #[cfg(unix)]
    #[tokio::test]
    async fn json_rpc_error_responses_are_forwarded_not_discarded() {
        if which::which("python3").is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("fake_lsp_error.py");
        std::fs::write(&server, FAKE_LSP_ERROR_PY).unwrap();
        let file = dir.path().join("main.xyz");
        std::fs::write(&file, "fn boom() {}\n").unwrap();

        let registry = LspRegistry::new(
            dir.path().to_path_buf(),
            vec![LspServerConfig {
                command: "python3".to_string(),
                args: vec![server.display().to_string()],
                extensions: vec!["xyz".to_string()],
            }],
            Some(5_000),
        );

        let err = registry
            .nav_request(
                "textDocument/rename",
                "renameProvider",
                &file,
                "fn boom() {}\n",
                (0, 3),
                json!({"newName": "blast"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot rename this symbol"),
            "the server's error.message must reach the caller, not just an \
             empty/generic failure: {err}"
        );
    }

    #[test]
    fn uris_round_trip_and_utf16_columns_convert() {
        let p = Path::new("/proj/src/a b.rs");
        assert_eq!(uri_to_path(&file_uri(p)).unwrap(), p);
        // Non-BMP char (🦀 = 2 UTF-16 units, 4 UTF-8 bytes).
        let line = "let 🦀 = boom;";
        let byte = line.find("boom").unwrap();
        let utf16 = byte_to_utf16_col(line, byte);
        assert_eq!(utf16, 9, "4 ascii + surrogate pair (2) + 3 ascii");
        assert_eq!(utf16_to_byte_col(line, utf16), byte);
    }

    /// Regression (MAJOR): a non-ASCII filename must round-trip through
    /// `file_uri`/`uri_to_path` intact. `uri_to_path` used to widen each raw
    /// UTF-8 byte to a `char` one at a time (`bytes[i] as char`, a Latin-1
    /// mapping, not UTF-8 decoding), so a multi-byte character split across
    /// that per-byte loop came out as mojibake. `file_uri` also now
    /// percent-encodes non-ASCII bytes (RFC 3986 — a URI is ASCII-only), so
    /// this also pins that the encoded form contains no raw UTF-8 bytes.
    #[test]
    fn non_ascii_filenames_round_trip_through_uris() {
        let p = Path::new("/proj/café.rs");
        let uri = file_uri(p);
        assert!(
            uri.is_ascii(),
            "a `file://` URI must be percent-encoded, not raw UTF-8: {uri}"
        );
        assert_eq!(uri, "file:///proj/caf%C3%A9.rs");
        assert_eq!(
            uri_to_path(&uri).unwrap(),
            p,
            "must decode back to the exact original path, not mojibake"
        );

        // A server that doesn't bother percent-encoding and sends the raw
        // UTF-8 bytes directly in the URI string must still decode correctly
        // — the bug produced `/proj/cafÃ©.rs` for exactly this input.
        let raw_uri = "file:///proj/café.rs";
        assert_eq!(uri_to_path(raw_uri).unwrap(), p);

        // A directory name with a space *and* a non-ASCII character together.
        let p2 = Path::new("/proj/my café/lib.rs");
        assert_eq!(uri_to_path(&file_uri(p2)).unwrap(), p2);
    }

    #[test]
    fn locations_parse_all_three_shapes() {
        let loc = serde_json::json!({"uri": "file:///p/a.rs",
            "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 6}}});
        let link = serde_json::json!({"targetUri": "file:///p/b.rs",
            "targetSelectionRange": {"start": {"line": 0, "character": 0},
                                     "end": {"line": 0, "character": 1}}});
        let single = parse_locations(&loc).unwrap();
        assert_eq!(single[0].path, PathBuf::from("/p/a.rs"));
        assert_eq!((single[0].line, single[0].column), (5, 3), "1-based");
        let many = parse_locations(&serde_json::json!([loc, link])).unwrap();
        assert_eq!(many.len(), 2);
        assert_eq!(many[1].path, PathBuf::from("/p/b.rs"));
        assert!(parse_locations(&Value::Null).unwrap().is_empty());
    }

    #[test]
    fn workspace_edits_parse_and_apply() {
        // The `changes` map form.
        let we = serde_json::json!({"changes": {"file:///p/a.rs": [
            {"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 8}},
             "newText": "blast"},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}},
             "newText": "blast"}
        ]}});
        let files = parse_workspace_edit(&we, Path::new("/")).unwrap();
        assert_eq!(files.len(), 1);
        let out = apply_lsp_edits("let boom = 1;\nboom();\n", &files[0].edits).unwrap();
        assert_eq!(out, "let blast = 1;\nblast();\n");

        // The `documentChanges` form; a file-operation entry is refused.
        let dc = serde_json::json!({"documentChanges": [
            {"textDocument": {"uri": "file:///p/a.rs", "version": 1},
             "edits": [{"range": {"start": {"line": 0, "character": 0},
                                  "end": {"line": 0, "character": 3}}, "newText": "new"}]}
        ]});
        assert_eq!(parse_workspace_edit(&dc, Path::new("/")).unwrap().len(), 1);
        let op = serde_json::json!({"documentChanges": [{"kind": "rename"}]});
        assert!(parse_workspace_edit(&op, Path::new("/")).is_err());

        // Overlapping edits are refused rather than corrupting the file.
        let overlap = vec![
            LspTextEdit {
                start: (0, 0),
                end: (0, 4),
                new_text: "x".into(),
            },
            LspTextEdit {
                start: (0, 2),
                end: (0, 6),
                new_text: "y".into(),
            },
        ];
        assert!(apply_lsp_edits("abcdefgh", &overlap).is_err());
    }

    /// The navigation tools end-to-end against the scripted server:
    /// definition + references resolve `symbol` on a 1-based line, and rename
    /// applies the server's WorkspaceEdit through the normal write path.
    #[cfg(unix)]
    #[tokio::test]
    async fn nav_tools_ride_the_language_server() {
        if which::which("python3").is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        use crate::Tool as _;
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("fake_lsp.py");
        std::fs::write(&server, FAKE_LSP_PY).unwrap();
        let file = dir.path().join("main.xyz");
        std::fs::write(&file, "boom here\nuse boom\n").unwrap();

        let mut ctx = crate::ToolContext::new(dir.path());
        ctx.lsp = Some(Arc::new(LspRegistry::new(
            dir.path().to_path_buf(),
            vec![LspServerConfig {
                command: "python3".to_string(),
                args: vec![server.display().to_string()],
                extensions: vec!["xyz".to_string()],
            }],
            Some(5_000),
        )));

        // Definition: symbol on line 2 resolves to the first occurrence.
        let out = crate::DefinitionTool
            .execute(
                serde_json::json!({"path": "main.xyz", "line": 2, "symbol": "boom"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("main.xyz:1:1"), "{out}");

        // References: both occurrences, counted.
        let out = crate::ReferencesTool
            .execute(
                serde_json::json!({"path": "main.xyz", "line": 1, "symbol": "boom"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.starts_with("2 reference(s):"), "{out}");
        assert!(out.contains("main.xyz:2:5"), "{out}");

        // A symbol that isn't on the line is a plain, actionable error.
        let err = crate::DefinitionTool
            .execute(
                serde_json::json!({"path": "main.xyz", "line": 1, "symbol": "nope"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not appear"), "{err}");

        // Rename: the server's edits land on disk through apply_file_change.
        let out = crate::RenameTool
            .execute(
                serde_json::json!({"path": "main.xyz", "line": 1, "symbol": "boom",
                                   "new_name": "blast"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.contains("Renamed `boom` → `blast` in 1 file(s)"),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "blast here\nuse blast\n"
        );
    }

    /// A server that initializes fine but then stops draining its stdin must
    /// not hang the edit: the bounded write times out, diagnostics degrade to
    /// `None` (the edit still succeeds), and the server is retired so later
    /// edits skip it fast instead of each re-hitting the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_wedged_server_times_out_and_is_retired() {
        if which::which("python3").is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("wedged_lsp.py");
        std::fs::write(&server, FAKE_LSP_WEDGED_PY).unwrap();
        let registry = LspRegistry::new(
            dir.path().to_path_buf(),
            vec![LspServerConfig {
                command: "python3".to_string(),
                args: vec![server.display().to_string()],
                extensions: vec!["xyz".to_string()],
            }],
            Some(500), // short write budget so the test is quick
        );

        // A body larger than the OS pipe buffer (~64KB) forces `write_all` to
        // block on a server that isn't reading — the wedge we're guarding.
        let big = "x\n".repeat(200_000);
        let file = dir.path().join("main.xyz");

        let start = std::time::Instant::now();
        let note = registry.diagnostics_note(&file, &big).await;
        assert_eq!(
            note, None,
            "a wedged server yields no diagnostics, not a hang"
        );
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "must not hang: took {:?}",
            start.elapsed()
        );

        // The server is retired; `/doctor` reports it failed and a second edit
        // returns immediately without re-hitting the write timeout.
        assert_eq!(registry.statuses().await[0].status, LspServerStatus::Failed);
        let start = std::time::Instant::now();
        assert_eq!(registry.diagnostics_note(&file, &big).await, None);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a retired server is skipped fast: took {:?}",
            start.elapsed()
        );
    }

    /// A minimal, protocol-correct LSP server: answers `initialize`
    /// (advertising the navigation capabilities), publishes one error per
    /// line containing "boom" on `didOpen`/`didChange` (with the document's
    /// version, exercising the stale-publish guard), and serves
    /// definition/references/rename for the word "boom" from the synced text.
    #[cfg(unix)] // used only by the unix-gated integration tests above
    const FAKE_LSP_PY: &str = r#"
import json, sys

texts = {}

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
    texts[uri] = text
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

def occurrences(uri):
    out = []
    for i, line in enumerate(texts.get(uri, "").splitlines()):
        start = 0
        while True:
            j = line.find("boom", start)
            if j < 0:
                break
            out.append((i, j))
            start = j + 4
    return out

def loc(uri, l, c):
    return {"uri": uri, "range": {"start": {"line": l, "character": c},
                                  "end": {"line": l, "character": c + 4}}}

while True:
    msg = read()
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {
            "definitionProvider": True,
            "referencesProvider": True,
            "renameProvider": True,
        }}})
    elif method == "textDocument/didOpen":
        d = msg["params"]["textDocument"]
        publish(d["uri"], d["version"], d["text"])
    elif method == "textDocument/didChange":
        d = msg["params"]["textDocument"]
        publish(d["uri"], d["version"], msg["params"]["contentChanges"][0]["text"])
    elif method == "textDocument/definition":
        uri = msg["params"]["textDocument"]["uri"]
        occ = occurrences(uri)
        result = loc(uri, *occ[0]) if occ else None
        send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
    elif method == "textDocument/references":
        uri = msg["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": [loc(uri, l, c) for (l, c) in occurrences(uri)]})
    elif method == "textDocument/rename":
        uri = msg["params"]["textDocument"]["uri"]
        new = msg["params"]["newName"]
        edits = [{"range": loc(uri, l, c)["range"], "newText": new}
                 for (l, c) in occurrences(uri)]
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": {"changes": {uri: edits}}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
"#;

    /// A server that answers `initialize` (advertising the navigation
    /// capabilities so the request isn't refused before it's even sent) and
    /// then answers every navigation request with a JSON-RPC `error` instead
    /// of a `result` — modelling a server refusing e.g. an unrenameable
    /// symbol, to exercise error-message forwarding.
    #[cfg(unix)]
    const FAKE_LSP_ERROR_PY: &str = r#"
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

while True:
    msg = read()
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {
            "definitionProvider": True,
            "referencesProvider": True,
            "renameProvider": True,
        }}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        pass
    elif method in ("textDocument/definition", "textDocument/references",
                     "textDocument/rename"):
        send({"jsonrpc": "2.0", "id": msg["id"],
              "error": {"code": -32602, "message": "cannot rename this symbol"}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
"#;

    /// A server that answers `initialize` (so `start` succeeds) and then stops
    /// reading its stdin entirely — modelling a crashed-but-not-exited / wedged
    /// server. The next large write fills the pipe and blocks, exercising the
    /// per-write timeout.
    #[cfg(unix)]
    const FAKE_LSP_WEDGED_PY: &str = r#"
import json, sys, time

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

msg = read()
if msg.get("method") == "initialize":
    send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
# Now go deaf: never read stdin again, so the next big write wedges.
time.sleep(3600)
"#;
}

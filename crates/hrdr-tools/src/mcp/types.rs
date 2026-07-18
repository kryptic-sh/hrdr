use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};

use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::{Mutex, oneshot, watch};

pub(crate) type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A live connection to one MCP server.
pub struct McpClient {
    pub(crate) server: String,
    pub(crate) next_id: AtomicU64,
    pub(crate) transport: Transport,
}

pub(crate) enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
    Sse(SseTransport),
}

/// stdio transport: a spawned child + a writer channel + the id→response map.
/// Dropping it kills the child (`kill_on_drop`) *and* every descendant it
/// forked (the [`ProcessGroup`](crate::proc::ProcessGroup) guard's `Drop`).
pub(crate) struct StdioTransport {
    pub(crate) stdin_tx: tokio::sync::mpsc::Sender<String>,
    pub(crate) pending: Pending,
    pub(crate) _child: Child,
    /// Owns the process group / job object `_child` was placed in. Declared
    /// after `_child` so it drops after it. `kill_on_drop` only reaps the
    /// leader pid; an MCP server launched through `npx`/`uvx`/a wrapper script
    /// forks its real server as a grandchild, which would otherwise survive
    /// teardown holding sockets and file locks. This guard's `Drop` takes the
    /// whole tree down — unix `kill(-pgid)`, Windows job-handle close.
    pub(crate) _group: Option<crate::proc::ProcessGroup>,
}

/// Streamable-HTTP transport: POST to `url`, carrying `headers` (auth) and the
/// server-assigned session id once known.
pub(crate) struct HttpTransport {
    pub(crate) http: reqwest::Client,
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
    pub(crate) session: StdMutex<Option<String>>,
}

/// Legacy HTTP+SSE transport: a persistent GET stream carries server→client
/// messages (and the initial `endpoint` event giving the POST URL); requests are
/// POSTed to that URL and their responses arrive back on the stream, routed by id
/// like stdio.
pub(crate) struct SseTransport {
    pub(crate) http: reqwest::Client,
    pub(crate) headers: HeaderMap,
    /// POST endpoint from the `endpoint` SSE event (`None` until received).
    pub(crate) post_url: watch::Receiver<Option<String>>,
    pub(crate) pending: Pending,
}

/// A tool advertised by an MCP server's `tools/list`.
pub(crate) struct Discovered {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) schema: Value,
    pub(crate) read_only: bool,
}

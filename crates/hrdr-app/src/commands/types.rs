use std::future::Future;
use std::pin::Pin;

/// A future producing a system line to display (empty = show nothing). The host
/// spawns it on its runtime and pipes the result to its transcript.
pub type LineFuture = Pin<Box<dyn Future<Output = String> + Send>>;

/// A future resolving a browser OAuth login to its [`BrowserLoginOutcome`]. The
/// worker only exchanges + saves the credential; it never switches the provider
/// or touches UI state. Wrapped in the ChatGPT backstop by its constructor.
pub type BrowserLoginFuture = Pin<Box<dyn Future<Output = BrowserLoginOutcome> + Send>>;

/// A launched browser OAuth login. The URL is displayed by `open_browser`
/// separately and is deliberately NOT carried here.
pub struct BrowserLoginStart {
    /// Monotonic id used to reject a stale/duplicate login's late result.
    pub login_id: u64,
    /// The provider being logged into (`chatgpt` or `openrouter`).
    pub provider: String,
    /// Exchange + save future (credential only — no provider switch).
    pub future: BrowserLoginFuture,
}

/// The result of a browser OAuth login's exchange/save step. The caller decides
/// what to do next (persist default, live-switch, refresh models) and owns
/// reporting; the worker only reports whether the credential was saved.
pub struct BrowserLoginOutcome {
    /// Matches the originating [`BrowserLoginStart::login_id`].
    pub login_id: u64,
    pub provider: String,
    /// Whether the credential was successfully exchanged and saved.
    pub token_saved: bool,
    /// A sanitized failure message when `token_saved` is false.
    pub error: Option<String>,
}

/// How an async result line should be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A plain system line.
    System,
    /// A unified diff (frontends with diff-aware rendering color it).
    Diff,
}

/// What `/expand` should do to tool output (parsed by the shared dispatcher;
/// applied by the frontend, which owns the expansion state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    /// Show every tool result in full.
    All,
    /// Collapse everything.
    Off,
    /// Toggle the most recent tool result.
    ToggleLast,
}
